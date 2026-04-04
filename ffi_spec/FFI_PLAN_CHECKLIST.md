# FFI PLAN Progress Checklist

## INSTRUCTIONS

Main plan: [FFI_PLAN.md](FFI_PLAN.md) - please read first.

Please progress the tasks in this document one phase at a time and one step at a time. Please keep the `STATUS` section updated with your current status and list any outstanding issues or blockers.

For each step we need to make sure the code passes tests and linting.

## STATUS

Starting Phase 1c. Phases 1 and 1b are substantially complete in core/FFI/Python (see verified status below). Phase 1c has no implementation yet — all items are TODO.

Outstanding blocker: None.

---

## Verified Baseline (2026-04-04)

Before starting new work, confirm the existing surface is healthy:

- [ ] `cargo test -p aster_transport_core` passes
- [ ] `cargo test -p aster_transport_ffi --test test_ffi` passes (expect 39 tests)
- [ ] `uv run pytest tests/python/test_phase1b.py -q` passes (expect 8 tests)
- [ ] `uv run pytest tests/python/ -q` passes (all existing Python tests)
- [ ] `uv run ruff check bindings/aster_python_rs/` passes (or N/A for Rust)

---

## Phase 1b — Remaining Gaps

These are items from Phase 1b that are partially implemented or missing Python exposure.

### Python: Doc Query Methods (core done, Python missing)

The core has `query_key_exact`, `query_key_prefix`, and `read_entry_content` on `CoreDoc`, but the Python `DocHandle` wrapper in `bindings/aster_python_rs/src/docs.rs` does not expose them.

- [ ] Add `query_key_exact(key: bytes) -> list[DocEntry]` to Python `DocHandle`
- [ ] Add `query_key_prefix(prefix: bytes) -> list[DocEntry]` to Python `DocHandle`
- [ ] Add `read_entry_content(content_hash_hex: str) -> bytes` to Python `DocHandle`
- [ ] Define Python `DocEntry` class (author_id, key, content_hash, content_len, timestamp)
- [ ] Add Python tests for doc query round-trip (write with author A, query by key, filter by author)
- [ ] Update `bindings/aster_python/__init__.pyi` type stubs

### FFI: Hook Reply Wiring (core done, FFI wiring incomplete)

The core `CoreHooksAdapter` and `CoreHookReceiver` are complete. The FFI event-queue push path is not wired.

- [ ] Wire FFI event queue to emit `IROH_EVENT_HOOK_BEFORE_CONNECT` from `CoreHookReceiver`
- [ ] Wire FFI event queue to emit `IROH_EVENT_HOOK_AFTER_CONNECT` from `CoreHookReceiver`
- [ ] Implement `iroh_hook_before_connect_respond` in FFI
- [ ] Implement `iroh_hook_after_connect_respond` in FFI
- [ ] Add FFI integration test: hook before_connect allow/deny
- [ ] Add FFI integration test: hook after_connect delivery

---

## Phase 1c — Registry & Publication Support

### 1c.1 Blob Tags (P0)

**Core (`aster_transport_core`):**

- [ ] Add `CoreTagInfo` struct (name, hash, format)
- [ ] Implement `CoreBlobsClient::tag_set(name, hash_hex, format)` — delegates to `store.tags().set()`
- [ ] Implement `CoreBlobsClient::tag_get(name)` — delegates to `store.tags().get()`
- [ ] Implement `CoreBlobsClient::tag_delete(name)` — delegates to `store.tags().delete()`
- [ ] Implement `CoreBlobsClient::tag_delete_prefix(prefix)` — delegates to `store.tags().delete_prefix()`
- [ ] Implement `CoreBlobsClient::tag_list()` — delegates to `store.tags().list()`
- [ ] Implement `CoreBlobsClient::tag_list_prefix(prefix)` — delegates to `store.tags().list_prefix()`
- [ ] Implement `CoreBlobsClient::tag_list_hash_seq()` — delegates to `store.tags().list_hash_seq()`
- [ ] Add Rust unit test: tag_set + tag_get round-trip
- [ ] Add Rust unit test: tag_delete removes tag
- [ ] Add Rust unit test: tag_list_prefix filters correctly

**Python (`bindings/aster_python_rs/src/blobs.rs`):**

- [ ] Add `TagInfo` Python class (name, hash, format)
- [ ] Expose `BlobsClient.tag_set(name, hash_hex, format)` as async method
- [ ] Expose `BlobsClient.tag_get(name)` as async method → `Optional[TagInfo]`
- [ ] Expose `BlobsClient.tag_delete(name)` as async method → `int`
- [ ] Expose `BlobsClient.tag_delete_prefix(prefix)` as async method → `int`
- [ ] Expose `BlobsClient.tag_list()` as async method → `list[TagInfo]`
- [ ] Expose `BlobsClient.tag_list_prefix(prefix)` as async method → `list[TagInfo]`
- [ ] Expose `BlobsClient.tag_list_hash_seq()` as async method → `list[TagInfo]`
- [ ] Add Python test: tag_set + tag_get round-trip
- [ ] Add Python test: tag_delete removes tag, tag_get returns None
- [ ] Add Python test: tag_list returns expected tags
- [ ] Update `bindings/aster_python/__init__.pyi` type stubs

**FFI (`aster_transport_ffi`):**

- [ ] Add `IROH_EVENT_TAG_SET` (36), `IROH_EVENT_TAG_GET` (37), `IROH_EVENT_TAG_DELETED` (38), `IROH_EVENT_TAG_LIST` (39) event kinds
- [ ] Implement `iroh_tags_set`
- [ ] Implement `iroh_tags_get`
- [ ] Implement `iroh_tags_delete`
- [ ] Implement `iroh_tags_list_prefix`
- [ ] Add FFI integration test: tag lifecycle

### 1c.2 Fix `add_bytes_as_collection` (P0)

- [ ] Replace `std::mem::forget(tag)` in `CoreBlobsClient::add_bytes_as_collection` with proper `tag_set`
- [ ] Replace `std::mem::forget(collection_tag)` with proper `tag_set` for the collection
- [ ] Verify existing blob/collection tests still pass after the change
- [ ] Add test: unpublish via `tag_delete` → blob is no longer served (or at least tag is gone)

### 1c.3 Blob Status / Has (P1)

**Core:**

- [ ] Add `CoreBlobStatus` enum (NotFound, Partial { size }, Complete { size })
- [ ] Implement `CoreBlobsClient::blob_status(hash_hex)` — delegates to `store.blobs().status()`
- [ ] Implement `CoreBlobsClient::blob_has(hash_hex)` — delegates to `store.blobs().has()`
- [ ] Add Rust unit test: blob_status for known blob returns Complete
- [ ] Add Rust unit test: blob_has for unknown blob returns false

**Python:**

- [ ] Expose `BlobsClient.blob_status(hash_hex)` → dict or enum-like
- [ ] Expose `BlobsClient.blob_has(hash_hex)` → `bool`
- [ ] Add Python test: blob_status after add_bytes returns complete
- [ ] Add Python test: blob_has for unknown hash returns False

**FFI:**

- [ ] Implement `iroh_blobs_status` (synchronous, writes to out params)
- [ ] Implement `iroh_blobs_has` (synchronous)
- [ ] Add FFI integration test: blob status/has

### 1c.4 Doc Subscribe — Live Events (P1)

**Core:**

- [ ] Add `CoreDocEvent` enum (InsertLocal, InsertRemote, ContentReady, NeighborUp, NeighborDown, SyncFinished)
- [ ] Add `CoreDocEventReceiver` struct wrapping iroh-docs event stream
- [ ] Implement `CoreDocEventReceiver::recv()` → `Option<CoreDocEvent>`
- [ ] Implement `CoreDoc::subscribe()` → `CoreDocEventReceiver`
- [ ] Add Rust test: subscribe, write entry, receive InsertLocal event

**Python:**

- [ ] Add `DocEvent` Python class hierarchy (or dict-based representation)
- [ ] Add `DocEventReceiver` Python class with async `recv()` method
- [ ] Expose `DocHandle.subscribe()` → `DocEventReceiver`
- [ ] Add Python test: subscribe + write → receive event

**FFI:**

- [ ] Add `IROH_EVENT_DOC_SUBSCRIBED` (47) and `IROH_EVENT_DOC_EVENT` (48) event kinds
- [ ] Implement `iroh_doc_subscribe`
- [ ] Implement `iroh_doc_event_recv`
- [ ] Add FFI integration test: doc subscribe + event delivery

### 1c.5 Doc Sync Lifecycle (P1)

**Core:**

- [ ] Implement `CoreDoc::start_sync(peers: Vec<String>)` — delegates to upstream `doc.start_sync()`
- [ ] Implement `CoreDoc::leave()` — delegates to upstream `doc.leave()`
- [ ] Add Rust test: start_sync + leave lifecycle

**Python:**

- [ ] Expose `DocHandle.start_sync(peers: list[str])` as async method
- [ ] Expose `DocHandle.leave()` as async method
- [ ] Add Python test: start_sync + leave lifecycle

**FFI:**

- [ ] Implement `iroh_doc_start_sync`
- [ ] Implement `iroh_doc_leave`
- [ ] Add FFI integration test: sync lifecycle

### 1c.6 Doc Download Policy (P2)

**Core:**

- [ ] Add `CoreDownloadPolicy` enum (Everything, NothingExcept { prefixes }, EverythingExcept { prefixes })
- [ ] Implement `CoreDoc::set_download_policy(policy)` — maps to upstream `DownloadPolicy`
- [ ] Implement `CoreDoc::get_download_policy()` — maps from upstream `DownloadPolicy`
- [ ] Add Rust test: set_download_policy + get_download_policy round-trip

**Python:**

- [ ] Expose `DocHandle.set_download_policy(mode, prefixes)` as async method
- [ ] Expose `DocHandle.get_download_policy()` as async method
- [ ] Add Python test: download policy round-trip

**FFI:**

- [ ] Add `iroh_download_policy_mode_t` enum
- [ ] Implement `iroh_doc_set_download_policy`
- [ ] Add FFI integration test: download policy

### 1c.7 Doc Share with Full Address (P2)

**Core:**

- [ ] Implement `CoreDoc::share_with_addr(mode)` — calls `doc.share(mode, AddrInfoOptions::RelayAndAddresses)`
- [ ] Add Rust test: share_with_addr produces ticket with relay URL

**Python:**

- [ ] Expose `DocHandle.share_with_addr(mode: str)` as async method → `str`
- [ ] Add Python test: share_with_addr returns valid ticket

**FFI:**

- [ ] Implement `iroh_doc_share_with_addr`
- [ ] Add FFI integration test: share_with_addr

### 1c.8 Doc Import and Subscribe (P2)

**Core:**

- [ ] Implement `CoreDocsClient::join_and_subscribe(ticket_str)` → `(CoreDoc, CoreDocEventReceiver)`
- [ ] Add Rust test: join_and_subscribe receives initial sync events

**Python:**

- [ ] Expose `DocsClient.join_and_subscribe(ticket: str)` → `(DocHandle, DocEventReceiver)`
- [ ] Add Python test: join_and_subscribe lifecycle

**FFI:**

- [ ] Implement `iroh_docs_join_and_subscribe`
- [ ] Add FFI integration test: join_and_subscribe

---

## Phase 1c — Final Verification

- [ ] All Phase 1c core methods have doc comments
- [ ] `cargo test -p aster_transport_core` passes with new tests
- [ ] `cargo test -p aster_transport_ffi --test test_ffi` passes with new tests
- [ ] `uv run pytest tests/python/ -q` passes with new tests
- [ ] `uv run ruff check bindings/` passes
- [ ] Update `Aster-ContractIdentity.md` §11.5 to mark completed items
- [ ] Update `FFI_PLAN.md` §3c.8 Priority Summary status column

---

## Milestone Summary

| Milestone | Status |
|-----------|--------|
| Baseline verification | ⬜ Not started |
| Phase 1b remaining (doc query Python, hook FFI wiring) | ⬜ Not started |
| Phase 1c P0: Blob Tags | ⬜ Not started |
| Phase 1c P0: Fix add_bytes_as_collection | ⬜ Not started |
| Phase 1c P1: Blob Status / Has | ⬜ Not started |
| Phase 1c P1: Doc Subscribe | ⬜ Not started |
| Phase 1c P1: Doc Sync Lifecycle | ⬜ Not started |
| Phase 1c P2: Doc Download Policy | ⬜ Not started |
| Phase 1c P2: Doc Share with Full Addr | ⬜ Not started |
| Phase 1c P2: Doc Import and Subscribe | ⬜ Not started |
| Final verification | ⬜ Not started |