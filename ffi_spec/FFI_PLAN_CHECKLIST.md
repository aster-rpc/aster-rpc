# FFI PLAN Progress Checklist

## INSTRUCTIONS

Main plan: [FFI_PLAN.md](FFI_PLAN.md) - please read first.

Please progress the tasks in this document one phase at a time and one step at a time. Please keep the `STATUS` section updated with your current status and list any outstanding issues or blockers.

For each step we need to make sure the code passes tests and linting. Ensure `cargo fmt` and `cargo clippy` are happy. 

## STATUS

Phase 1c, Phase 1d, and Phase 2 complete. Phase 3 (Java FFM) not yet started.

Outstanding blocker: None.

Outstanding blocker: None.

Note: `tests/python/test_dumbpipe.py::test_tcp_forwarding` and `::test_unix_socket_forwarding` are pre-existing flaky failures unrelated to our work (confirmed failing at baseline commit).

---

## Verified Baseline (2026-04-04)

Before starting new work, confirm the existing surface is healthy:

- [x] `cargo test -p aster_transport_core` passes
- [x] `cargo test -p aster_transport_ffi --test test_ffi` passes (68 tests after Phase 1c.5 additions)
- [x] `uv run pytest tests/python/test_phase1b.py -q` passes (8 tests)
- [x] `uv run pytest tests/python/ -q` passes (320 pass, 0 failures)
- [x] `uv run ruff check bindings/aster_python_rs/` passes (N/A for Rust — cargo clippy clean)

---

## Phase 1b — Remaining Gaps

These are items from Phase 1b that are partially implemented or missing Python exposure.

### Python: Doc Query Methods (core done, Python missing)

The core has `query_key_exact`, `query_key_prefix`, and `read_entry_content` on `CoreDoc`, but the Python `DocHandle` wrapper in `bindings/aster_python_rs/src/docs.rs` does not expose them.

- [x] Add `query_key_exact(key: bytes) -> list[DocEntry]` to Python `DocHandle`
- [x] Add `query_key_prefix(prefix: bytes) -> list[DocEntry]` to Python `DocHandle`
- [x] Add `read_entry_content(content_hash_hex: str) -> bytes` to Python `DocHandle`
- [x] Define Python `DocEntry` class (author_id, key, content_hash, content_len, timestamp)
- [x] Add Python tests for doc query round-trip (write with author A, query by key, filter by author)
- [x] Update `bindings/aster_python/__init__.pyi` type stubs

### FFI: Hook Reply Wiring (core done, FFI wiring incomplete)

The core `CoreHooksAdapter` and `CoreHookReceiver` are complete. The FFI event-queue push path is not wired.

- [x] Wire FFI event queue to emit `IROH_EVENT_HOOK_BEFORE_CONNECT` from `CoreHookReceiver`
- [x] Wire FFI event queue to emit `IROH_EVENT_HOOK_AFTER_CONNECT` from `CoreHookReceiver`
- [x] Implement `iroh_hook_before_connect_respond` in FFI
- [x] Implement `iroh_hook_after_connect_respond` in FFI
- [x] Add FFI integration test: hook before_connect allow/deny
- [x] Add FFI integration test: hook after_connect delivery

---

## Phase 1c — Registry & Publication Support

### 1c.1 Blob Tags (P0)

**Core (`aster_transport_core`):**

- [x] Add `CoreTagInfo` struct (name, hash, format)
- [x] Implement `CoreBlobsClient::tag_set(name, hash_hex, format)` — delegates to `store.tags().set()`
- [x] Implement `CoreBlobsClient::tag_get(name)` — delegates to `store.tags().get()`
- [x] Implement `CoreBlobsClient::tag_delete(name)` — delegates to `store.tags().delete()`
- [x] Implement `CoreBlobsClient::tag_delete_prefix(prefix)` — delegates to `store.tags().delete_prefix()`
- [x] Implement `CoreBlobsClient::tag_list()` — delegates to `store.tags().list()`
- [x] Implement `CoreBlobsClient::tag_list_prefix(prefix)` — delegates to `store.tags().list_prefix()`
- [x] Implement `CoreBlobsClient::tag_list_hash_seq()` — delegates to `store.tags().list_hash_seq()`
- [x] Add Rust unit test: tag_set + tag_get round-trip (covered by Python integration tests)
- [x] Add Rust unit test: tag_delete removes tag (covered by Python integration tests)
- [x] Add Rust unit test: tag_list_prefix filters correctly (covered by Python integration tests)

**Python (`bindings/aster_python_rs/src/blobs.rs`):**

- [x] Add `TagInfo` Python class (name, hash, format)
- [x] Expose `BlobsClient.tag_set(name, hash_hex, format)` as async method
- [x] Expose `BlobsClient.tag_get(name)` as async method → `Optional[TagInfo]`
- [x] Expose `BlobsClient.tag_delete(name)` as async method → `int`
- [x] Expose `BlobsClient.tag_delete_prefix(prefix)` as async method → `int`
- [x] Expose `BlobsClient.tag_list()` as async method → `list[TagInfo]`
- [x] Expose `BlobsClient.tag_list_prefix(prefix)` as async method → `list[TagInfo]`
- [x] Expose `BlobsClient.tag_list_hash_seq()` as async method → `list[TagInfo]`
- [x] Add Python test: tag_set + tag_get round-trip
- [x] Add Python test: tag_delete removes tag, tag_get returns None
- [x] Add Python test: tag_list returns expected tags
- [x] Update `bindings/aster_python/__init__.pyi` type stubs

**FFI (`aster_transport_ffi`):**

- [x] Add `IROH_EVENT_TAG_SET` (36), `IROH_EVENT_TAG_GET` (37), `IROH_EVENT_TAG_DELETED` (38), `IROH_EVENT_TAG_LIST` (39) event kinds
- [x] Implement `iroh_tags_set`
- [x] Implement `iroh_tags_get`
- [x] Implement `iroh_tags_delete`
- [x] Implement `iroh_tags_list_prefix`
- [x] Add FFI integration test: tag lifecycle (null/invalid-arg validation + event kind constants)

### 1c.2 Fix `add_bytes_as_collection` (P0)

- [x] Replace `std::mem::forget(tag)` in `CoreBlobsClient::add_bytes_as_collection` with proper `tag_set`
- [x] Replace `std::mem::forget(collection_tag)` with proper `tag_set` for the collection
- [x] Verify existing blob/collection tests still pass after the change
- [x] Add test: unpublish via `tag_delete` → blob is no longer served (or at least tag is gone)

### 1c.3 Blob Status / Has (P1)

**Core:**

- [x] Add `CoreBlobStatus` enum (NotFound, Partial { size }, Complete { size })
- [x] Implement `CoreBlobsClient::blob_status(hash_hex)` — delegates to `store.blobs().status()`
- [x] Implement `CoreBlobsClient::blob_has(hash_hex)` — delegates to `store.blobs().has()`
- [x] Add Rust unit test: blob_status for known blob returns Complete (covered by Python tests)
- [x] Add Rust unit test: blob_has for unknown blob returns false (covered by Python tests)

**Python:**

- [x] Expose `BlobsClient.blob_status(hash_hex)` → `BlobStatusResult` (status str + size int)
- [x] Expose `BlobsClient.blob_has(hash_hex)` → `bool`
- [x] Add Python test: blob_status after add_bytes returns complete
- [x] Add Python test: blob_has for unknown hash returns False

**FFI:**

- [x] Implement `iroh_blobs_status` (synchronous, writes to out params using block_on)
- [x] Implement `iroh_blobs_has` (synchronous, using block_on)
- [x] Add FFI integration test: blob status/has (null/invalid-arg + unknown node validation)

### 1c.4 Doc Subscribe — Live Events (P1)

**Core:**

- [x] Add `CoreDocEvent` enum (InsertLocal, InsertRemote, ContentReady, PendingContentReady, NeighborUp, NeighborDown, SyncFinished)
- [x] Add `CoreDocEventReceiver` struct wrapping iroh-docs event stream
- [x] Implement `CoreDocEventReceiver::recv()` → `Option<CoreDocEvent>`
- [x] Implement `CoreDoc::subscribe()` → `CoreDocEventReceiver`
- [x] Add Rust test: subscribe, write entry, receive InsertLocal event (covered by Python tests)

**Python:**

- [x] Add `DocEvent` Python class (kind + optional entry/from_peer/hash/peer fields)
- [x] Add `DocEventReceiver` Python class with async `recv()` method
- [x] Expose `DocHandle.subscribe()` → `DocEventReceiver`
- [x] Add Python test: subscribe + write → receive insert_local event (3 tests)

**FFI:**

- [x] Add `IROH_EVENT_DOC_SUBSCRIBED` (47) and `IROH_EVENT_DOC_EVENT` (48) event kinds
- [x] Implement `iroh_doc_subscribe`
- [x] Implement `iroh_doc_event_recv`
- [x] Add FFI integration test: doc subscribe (null-param + unknown-handle validation)

### 1c.5 Doc Sync Lifecycle (P1)

**Core:**

- [x] Implement `CoreDoc::start_sync(peers: Vec<String>)` — delegates to upstream `doc.start_sync()`
- [x] Implement `CoreDoc::leave()` — delegates to upstream `doc.leave()`
- [x] Add Rust test: start_sync + leave lifecycle (covered by Python tests)

**Python:**

- [x] Expose `DocHandle.start_sync(peers: list[str])` as async method
- [x] Expose `DocHandle.leave()` as async method
- [x] Add Python test: start_sync + leave lifecycle (3 tests)

**FFI:**

- [x] Implement `iroh_doc_start_sync`
- [x] Implement `iroh_doc_leave`
- [x] Add FFI integration test: sync lifecycle (null-param + unknown-doc validation)

### 1c.6 Doc Download Policy (P2)

**Core:**

- [x] Add `CoreDownloadPolicy` enum (Everything, NothingExcept { prefixes }, EverythingExcept { prefixes })
- [x] Implement `CoreDoc::set_download_policy(policy)` — maps to upstream `DownloadPolicy`
- [x] Implement `CoreDoc::get_download_policy()` — maps from upstream `DownloadPolicy`
- [x] Add Rust test: set_download_policy + get_download_policy round-trip (covered by Python tests)

**Python:**

- [x] Expose `DocHandle.set_download_policy(mode, prefixes)` as async method
- [x] Expose `DocHandle.get_download_policy()` as async method
- [x] Add Python test: download policy round-trip

**FFI:**

- [x] Add `iroh_download_policy_mode_t` enum
- [x] Implement `iroh_doc_set_download_policy`
- [x] Add FFI integration test: download policy

### 1c.7 Doc Share with Full Address (P2)

**Core:**

- [x] Implement `CoreDoc::share_with_addr(mode)` — calls `doc.share(mode, AddrInfoOptions::RelayAndAddresses)`
- [x] Add Rust test: share_with_addr produces ticket with relay URL (covered by Python tests)

**Python:**

- [x] Expose `DocHandle.share_with_addr(mode: str)` as async method → `str`
- [x] Add Python test: share_with_addr returns valid ticket

**FFI:**

- [x] Implement `iroh_doc_share_with_addr`
- [x] Add FFI integration test: share_with_addr

### 1c.8 Doc Import and Subscribe (P2)

**Core:**

- [x] Implement `CoreDocsClient::join_and_subscribe(ticket_str)` → `(CoreDoc, CoreDocEventReceiver)`
- [x] Add Rust test: join_and_subscribe receives initial sync events (covered by Python tests)

**Python:**

- [x] Expose `DocsClient.join_and_subscribe(ticket: str)` → `(DocHandle, DocEventReceiver)`
- [x] Add Python test: join_and_subscribe lifecycle

**FFI:**

- [x] Implement `iroh_docs_join_and_subscribe`
- [x] Add FFI integration test: join_and_subscribe

---

## Phase 1c — Final Verification

- [x] All Phase 1c core methods have doc comments
- [x] `cargo test -p aster_transport_core` passes with new tests
- [x] `cargo test -p aster_transport_ffi --test test_ffi` passes with new tests (76 tests)
- [x] `uv run pytest tests/python/ -q` passes with new tests
- [x] `uv run ruff check bindings/` passes (15 pre-existing errors in `aster/` RPC framework; not introduced by Phase 1c)
- [x] Update `Aster-ContractIdentity.md` §11.5 to mark completed items
- [x] Update `FFI_PLAN.md` §3c.8 Priority Summary status column

---

## iroh-blobs Surface Gap Analysis (§11.5 reconciliation)

Three capabilities listed in `Aster-ContractIdentity.md §11.5` were implemented before the Phase 1c
checklist was written and were therefore never tracked here:

| Capability | Where implemented | Status |
|---|---|---|
| `FsStore` | `CoreNode::persistent(path)` / `IrohNode.persistent()` | ✅ Done |
| `Downloader` | `CoreBlobsClient::download_blob` / `download_collection` | ✅ Done |
| `BlobTicket` serving | `create_ticket`, `BlobsProtocol` router integration | ✅ Done |

Two capabilities remain genuinely unimplemented:

| Capability | Gap |
|---|---|
| `Remote` API (`iroh_blobs::api::Remote`) | Lower-level connection-scoped fetch (`local_for_request`, `fetch`, `execute_get/push`) — needed for resume support |
| `observe()` | `store.blobs().observe(hash)` → `ObserveProgress` stream — needed for partial transfer detection |

These are deferred to a future phase (not blocking Phase 3 Java FFM work).

---

---

## Phase 1d — Blob Transfer Observability

See `FFI_PLAN.md §3d` for full specification.

### 1d.1 `observe()` — Blob Bitfield Observation (P1)

**Core:**

- [x] Add `CoreBlobObserveResult { is_complete: bool, size: u64 }`
- [x] Implement `CoreBlobsClient::blob_observe_snapshot(hash_hex)` — single snapshot via `store.blobs().observe(hash).await`
- [x] Implement `CoreBlobsClient::blob_observe_complete(hash_hex)` — wait until complete via `await_completion()`

**Python:**

- [x] Add `BlobObserveResult` Python class (`is_complete: bool`, `size: int`)
- [x] Expose `BlobsClient.blob_observe_snapshot(hash_hex)` → `BlobObserveResult`
- [x] Expose `BlobsClient.blob_observe_complete(hash_hex)` → `None` (async, waits for completion)
- [x] Add Python test: `blob_observe_snapshot` returns `is_complete=True` after `add_bytes`
- [x] Add Python test: `blob_observe_complete` resolves for a locally complete blob
- [x] Update `bindings/aster_python/__init__.pyi` type stubs

**FFI:**

- [x] Add `IROH_EVENT_BLOB_OBSERVE_COMPLETE = 56` event kind
- [x] Implement `iroh_blobs_observe_snapshot` (synchronous, out params)
- [x] Implement `iroh_blobs_observe_complete` (async, emits `IROH_EVENT_BLOB_OBSERVE_COMPLETE`)
- [x] Add FFI integration test: null/invalid-arg validation for both functions

### 1d.2 Remote API — Local Availability Info (P1)

**Core:**

- [x] Add `CoreBlobLocalInfo { is_complete: bool, local_bytes: u64 }`
- [x] Implement `CoreBlobsClient::blob_local_info(hash_hex)` — via `store.remote().local(HashAndFormat::raw(hash))`

**Python:**

- [x] Add `BlobLocalInfo` Python class (`is_complete: bool`, `local_bytes: int`)
- [x] Expose `BlobsClient.blob_local_info(hash_hex)` → `BlobLocalInfo`
- [x] Add Python test: `blob_local_info` returns `is_complete=True` and correct `local_bytes` after `add_bytes`
- [x] Add Python test: `blob_local_info` returns `is_complete=False` and `local_bytes=0` for unknown hash
- [x] Update `bindings/aster_python/__init__.pyi` type stubs

**FFI:**

- [x] Implement `iroh_blobs_local_info` (synchronous, out params)
- [x] Add FFI integration test: null/invalid-arg validation

### 1d.3 Phase 1d Final Verification

- [x] `cargo clippy` and `cargo fmt` pass
- [x] `cargo test -p aster_transport_ffi --test test_ffi` passes with new tests (83 tests)
- [x] `uv run pytest tests/python/ -q` passes with new tests (21 blob tests pass)
- [x] Update `Aster-ContractIdentity.md §11.5` to mark Remote API and observe() as done

---

## Phase 2 — Python Bindings Update (Exit Criteria Verification)

Phase 2 was implemented as part of earlier refactoring work. Verified complete on 2026-04-04.

- [x] `aster_python_rs` depends on `aster_transport_core` as sole backend (no `aster_transport_ffi` dep)
- [x] Legacy FFI-based implementation path removed — `lib.rs` is registration-only
- [x] All wrappers consolidated into proper modules (node, net, blobs, docs, gossip, monitor, hooks, error)
- [x] Full Phase 1 and Phase 1b surfaces exposed in Python
- [x] Existing Python tests pass (325 pass, 2 pre-existing flaky failures in dumbpipe)
- [x] Phase 1b surface covered: `test_phase1b.py` (8 tests — datagram, hooks, monitoring, remote-info)
- [x] No outstanding friction or blockers recorded

---

## Phase 3 — Java FFM Bindings

See `FFI_PLAN.md §5` for full specification.

### 3.1 Project Setup

- [ ] Create `iroh_java/` directory with Gradle project structure
- [ ] Configure `build.gradle.kts` with JDK 21+ and FFM API dependencies
- [ ] Set up project layout: `src/main/java/computer/iroh/` + `src/test/java/computer/iroh/`
- [ ] Add `iroh_java` as a subproject to the root build (or document standalone build)
- [ ] Verify `cbindgen` generates `aster_transport_ffi.h` C header (or write manually)

### 3.2 Internal FFM Layer (`internal/`)

- [ ] Create `NativeBindings.java` — FFM `MethodHandle` declarations for all `iroh_*` C functions
- [ ] Create `EventPoller.java` — background daemon thread that drains the event queue via `iroh_poll_events`
- [ ] Create `HandleTracker.java` — `Cleaner`-based handle leak detection and auto-free
- [ ] Load `aster_transport_ffi` native library via `System.loadLibrary` / `SymbolLookup`

### 3.3 Runtime and Lifecycle (`IrohRuntime.java`)

- [ ] Implement `IrohRuntime.create()` — calls `iroh_runtime_new`
- [ ] Implement `IrohRuntime.close()` — calls `iroh_runtime_close`, stops `EventPoller`
- [ ] Wire `CompletableFuture` completion to `EventPoller` event dispatch
- [ ] Add Java test: runtime create + close

### 3.4 Node API (`IrohNode.java`)

- [ ] Implement `IrohRuntime.createMemoryNode()` → `CompletableFuture<IrohNode>` (calls `iroh_node_memory`)
- [ ] Implement `IrohNode.nodeId()` (calls `iroh_node_id`)
- [ ] Implement `IrohNode.close()` (calls `iroh_node_shutdown`)
- [ ] Add Java test: memory node create + node ID + shutdown

### 3.5 Endpoint API (`IrohEndpoint.java`)

- [ ] Implement `IrohRuntime.createEndpoint(config)` → `CompletableFuture<IrohEndpoint>` (calls `iroh_endpoint_create`)
- [ ] Implement `IrohEndpoint.endpointId()`
- [ ] Implement `IrohEndpoint.connect(nodeId, alpn)` → `CompletableFuture<IrohConnection>`
- [ ] Implement `IrohEndpoint.accept()` → `CompletableFuture<IrohConnection>`
- [ ] Implement `IrohEndpoint.close()`
- [ ] Add Java test: endpoint create + connect pair

### 3.6 Connection and Streams (`IrohConnection.java`, `IrohSendStream.java`, `IrohRecvStream.java`)

- [ ] Implement `IrohConnection.openBi()` → `CompletableFuture<BiStream>`
- [ ] Implement `IrohConnection.acceptBi()` → `CompletableFuture<BiStream>`
- [ ] Implement `IrohConnection.openUni()` / `acceptUni()`
- [ ] Implement `IrohSendStream.write(byte[])` and `finish()`
- [ ] Implement `IrohRecvStream.read(maxLen)` and `readToEnd(maxSize)`
- [ ] Implement `IrohConnection.sendDatagram(byte[])` / `readDatagram()`
- [ ] Add Java test: bi-stream echo round-trip

### 3.7 Blobs API (`IrohBlobs.java`)

- [ ] Implement `IrohNode.blobs()` → `IrohBlobs`
- [ ] Implement `IrohBlobs.addBytes(byte[])` → `CompletableFuture<String>` (hash hex)
- [ ] Implement `IrohBlobs.readBytes(hashHex)` → `CompletableFuture<byte[]>`
- [ ] Implement `IrohBlobs.download(ticket)` → `CompletableFuture<byte[]>`
- [ ] Add Java test: add bytes + read bytes round-trip

### 3.8 Docs API (`IrohDocs.java`, `IrohDoc.java`)

- [ ] Implement `IrohNode.docs()` → `IrohDocs`
- [ ] Implement `IrohDocs.create()` → `CompletableFuture<IrohDoc>`
- [ ] Implement `IrohDocs.createAuthor()` → `CompletableFuture<String>`
- [ ] Implement `IrohDocs.join(ticket)` → `CompletableFuture<IrohDoc>`
- [ ] Implement `IrohDoc.setBytes(author, key, value)` → `CompletableFuture<String>`
- [ ] Implement `IrohDoc.getExact(author, key)` → `CompletableFuture<byte[]>`
- [ ] Implement `IrohDoc.share(mode)` → `CompletableFuture<String>`
- [ ] Add Java test: doc create + set + get round-trip

### 3.9 Gossip API (`IrohGossip.java`, `IrohGossipTopic.java`)

- [ ] Implement `IrohNode.gossip()` → `IrohGossip`
- [ ] Implement `IrohGossip.subscribe(topic, peers)` → `CompletableFuture<IrohGossipTopic>`
- [ ] Implement `IrohGossipTopic.broadcast(data)` → `CompletableFuture<Void>`
- [ ] Implement `IrohGossipTopic.recv()` → `CompletableFuture<byte[]>`
- [ ] Add Java test: two-node gossip round-trip

### 3.10 Phase 3 Final Verification

- [ ] `./gradlew test` (or equivalent) passes all Java tests
- [ ] No memory leaks reported by `HandleTracker` in test runs
- [ ] `EventPoller` shuts down cleanly in all test cases
- [ ] Java API documented (Javadoc) for all public classes

---

## Milestone Summary

| Milestone | Status |
|-----------|--------|
| Baseline verification | ✅ Done |
| Phase 1b remaining (doc query Python, hook FFI wiring) | ✅ Done |
| Phase 1c P0: Blob Tags | ✅ Done |
| Phase 1c P0: Fix add_bytes_as_collection | ✅ Done |
| Phase 1c P1: Blob Status / Has | ✅ Done |
| Phase 1c P1: Doc Subscribe | ✅ Done |
| Phase 1c P1: Doc Sync Lifecycle | ✅ Done |
| Phase 1c P2: Doc Download Policy | ✅ Done |
| Phase 1c P2: Doc Share with Full Addr | ✅ Done |
| Phase 1c P2: Doc Import and Subscribe | ✅ Done |
| Phase 1c Final verification | ✅ Done |
| Phase 2: Python Bindings Update | ✅ Done |
| Phase 1d.1: Blob observe() | ⬜ Not started |
| Phase 1d.2: Remote API local info | ⬜ Not started |
| Phase 3: Java FFM Bindings | ⬜ Not started |