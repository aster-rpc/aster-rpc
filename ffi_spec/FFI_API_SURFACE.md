# FFI Public API Surface

**Status:** Living document  
**Date:** 2026-04-12  
**Purpose:** Define which C FFI functions constitute the public API, classify them by performance criticality, and track binding coverage across languages.

---

## Why This Document Exists

The FFI layer exposes 108 C functions. Performance optimization work (zero-copy, SPSC ring tuning, poll batching) is expensive to apply broadly. This document scopes that work by classifying every function into one of three categories:

1. **Hot path** — called per-RPC or per-message. Every nanosecond matters.
2. **Warm path** — called per-connection or periodically (seconds). Should be efficient but not micro-optimized.
3. **Cold path** — called once at startup/shutdown or rarely. Correctness only; no perf work needed.

---

## Hot Path (per-RPC, per-message)

These 8 functions are on the critical data path for every Aster RPC. FFI crossing cost, buffer allocation, and memory copy overhead directly impact throughput and latency.

| Function | Type | Description | Optimization Target |
|----------|------|-------------|---------------------|
| `aster_reactor_poll` | Sync | Drain calls from SPSC ring | Zero-copy ring read, batch amortization |
| `aster_reactor_submit` | Sync | Submit response for a call | Minimize copy into response channel |
| `aster_reactor_buffer_release` | Sync | Release call payload buffers | Lock-free release path |
| `iroh_stream_write` | Async | Write data to QUIC stream | Avoid Bytes allocation per write |
| `iroh_stream_read` | Async | Read data from QUIC stream | read_into caller buffer (aster-rpc/noq) |
| `iroh_stream_finish` | Async | Signal end of stream | — |
| `iroh_poll_events` | Sync | Drain completion queue | Batch drain, cache-friendly layout |
| `iroh_buffer_release` | Sync | Release event data buffer | Lock-free release path |

**Note:** `iroh_stream_read` and `iroh_stream_write` are hot path when users build streaming applications directly on streams. For Aster RPC, the reactor handles stream I/O internally — the binding never calls these per-RPC. They remain hot path for non-reactor use cases (e.g., blob transfer, custom protocols).

---

## Warm Path (per-connection, periodic)

Called when connections open/close or on timer intervals (10-60s). Should be reasonably fast but not the focus of micro-optimization.

### Connection lifecycle

| Function | Type | Description |
|----------|------|-------------|
| `iroh_connect` | Async | Connect to remote peer |
| `iroh_accept` | Async | Accept incoming connection |
| `iroh_open_bi` | Async | Open bidirectional stream |
| `iroh_accept_bi` | Async | Accept bidirectional stream |
| `iroh_connection_close` | Sync | Close a connection |
| `iroh_connection_remote_id` | Sync | Get peer's node ID |
| `iroh_add_node_addr` | Sync | Register peer address |

### HA: Registry doc operations (every 10-30s)

| Function | Type | HA Role |
|----------|------|---------|
| `iroh_doc_set_bytes` | Async | Producer writes lease entry |
| `iroh_doc_get_exact` | Async | Consumer reads specific lease |
| `iroh_doc_query` | Async | Consumer scans all leases |
| `iroh_doc_subscribe` | Async | Consumer watches for changes |
| `iroh_doc_event_recv` | Async | Consumer polls for doc events |

### HA: Gossip operations (every 10s)

| Function | Type | HA Role |
|----------|------|---------|
| `iroh_gossip_broadcast` | Async | Producer sends detailed metrics |
| `iroh_gossip_recv` | Async | Producer receives peer metrics |

### Blob data path (app-dependent)

| Function | Type | Notes |
|----------|------|-------|
| `iroh_blobs_add_bytes` | Async | Could be frequent in file-transfer apps |
| `iroh_blobs_read` | Async | Could be frequent in file-transfer apps |
| `iroh_blobs_download` | Async | Network-bound, not FFI-bound |

---

## Cold Path (startup, shutdown, one-time)

Called once or rarely. No performance work needed — correctness only.

### Runtime and node lifecycle

| Function | Type | When Called |
|----------|------|------------|
| `iroh_runtime_new` | Sync | Once at process start |
| `iroh_runtime_close` | Sync | Once at shutdown |
| `iroh_node_memory` | Async | Once per node |
| `iroh_node_memory_with_alpns` | Async | Once per node |
| `iroh_node_persistent` | Async | Once per node |
| `iroh_node_close` | Async | Once at shutdown |
| `iroh_node_free` | Sync | Once at shutdown |
| `iroh_node_id` | Sync | Once after node creation |
| `iroh_node_addr_info` | Sync | Once for address exchange |
| `iroh_node_export_secret_key` | Sync | Rare |
| `iroh_node_accept_aster` | Async | Once per accept loop |
| `iroh_secret_key_generate` | Sync | Once |

### Endpoint lifecycle

| Function | Type | When Called |
|----------|------|------------|
| `iroh_endpoint_create` | Async | Once per endpoint |
| `iroh_endpoint_close` | Async | Once at shutdown |
| `iroh_endpoint_free` | Sync | Once at shutdown |
| `iroh_endpoint_id` | Sync | Once after creation |
| `iroh_endpoint_addr_info` | Sync | Once for address exchange |
| `iroh_endpoint_export_secret_key` | Sync | Rare |
| `iroh_endpoint_remote_info` | Sync | Diagnostic |
| `iroh_endpoint_remote_info_list` | Sync | Diagnostic |
| `iroh_endpoint_transport_metrics` | Sync | Diagnostic |

### Reactor lifecycle

| Function | Type | When Called |
|----------|------|------------|
| `aster_reactor_create` | Sync | Once per server |
| `aster_reactor_destroy` | Sync | Once at shutdown |

### Doc/Gossip lifecycle

| Function | Type | When Called |
|----------|------|------------|
| `iroh_docs_create` | Async | Once per document |
| `iroh_docs_create_author` | Async | Once per author |
| `iroh_docs_join` | Async | Once per join |
| `iroh_docs_join_and_subscribe` | Async | Once per join |
| `iroh_doc_share` | Async | Once per share |
| `iroh_doc_share_with_addr` | Async | Once per share |
| `iroh_doc_start_sync` | Async | Once per sync session |
| `iroh_doc_leave` | Async | Once per leave |
| `iroh_doc_set_download_policy` | Async | Once per policy change |
| `iroh_doc_read_entry_content` | Async | Per-entry read (infrequent) |
| `iroh_doc_free` | Sync | Once per doc close |
| `iroh_gossip_subscribe` | Async | Once per topic |
| `iroh_gossip_topic_free` | Sync | Once per topic close |

### Blob lifecycle and metadata

| Function | Type | When Called |
|----------|------|------------|
| `iroh_blobs_add_bytes_as_collection` | Async | Infrequent |
| `iroh_blobs_add_collection` | Async | Infrequent |
| `iroh_blobs_list_collection` | Async | Infrequent |
| `iroh_blobs_create_ticket` | Sync | Infrequent |
| `iroh_blobs_create_collection_ticket` | Sync | Infrequent |
| `iroh_blobs_status` | Sync | Polling, infrequent |
| `iroh_blobs_has` | Sync | Polling, infrequent |
| `iroh_blobs_observe_snapshot` | Sync | Diagnostic |
| `iroh_blobs_observe_complete` | Async | Once per download wait |
| `iroh_blobs_local_info` | Sync | Diagnostic |

### Tags

| Function | Type | When Called |
|----------|------|------------|
| `iroh_tags_set` | Async | Infrequent |
| `iroh_tags_get` | Async | Infrequent |
| `iroh_tags_delete` | Async | Infrequent |
| `iroh_tags_list_prefix` | Async | Infrequent |

### Stream extras

| Function | Type | When Called |
|----------|------|------------|
| `iroh_open_uni` | Async | Rare in Aster (bi is standard) |
| `iroh_accept_uni` | Async | Rare |
| `iroh_stream_read_to_end` | Async | Convenience, not hot path |
| `iroh_stream_read_exact` | Async | Convenience |
| `iroh_stream_stopped` | Async | Error handling |
| `iroh_stream_stop` | Sync | Error handling |
| `iroh_send_stream_free` | Sync | Per-stream cleanup |
| `iroh_recv_stream_free` | Sync | Per-stream cleanup |

### Connection extras

| Function | Type | When Called |
|----------|------|------------|
| `iroh_connection_closed` | Async | Once per connection end |
| `iroh_connection_free` | Sync | Per-connection cleanup |
| `iroh_connection_send_datagram` | Sync | App-specific |
| `iroh_connection_read_datagram` | Async | App-specific |
| `iroh_connection_max_datagram_size` | Sync | Diagnostic |
| `iroh_connection_datagram_send_buffer_space` | Sync | Diagnostic |
| `iroh_connection_info` | Sync | Diagnostic |

### Hooks

| Function | Type | When Called |
|----------|------|------------|
| `iroh_hook_before_connect_respond` | Sync | Per-connection (trust gate) |
| `iroh_hook_after_connect_respond` | Sync | Per-connection |

### Aster utilities

| Function | Type | When Called |
|----------|------|------------|
| `aster_contract_id` | Sync | Once per contract registration |
| `aster_canonical_bytes` | Sync | Per-signing operation |
| `aster_frame_encode` | Sync | Per-frame (but reactor handles this internally) |
| `aster_frame_decode` | Sync | Per-frame (but reactor handles this internally) |
| `aster_signing_bytes` | Sync | Per-signing operation |
| `aster_canonical_json` | Sync | Per-contract operation |
| `aster_ticket_encode` | Sync | Per-ticket creation |
| `aster_ticket_decode` | Sync | Per-ticket parsing |

### Error and version

| Function | Type | When Called |
|----------|------|------------|
| `iroh_abi_version_major` | Sync | Once at init |
| `iroh_abi_version_minor` | Sync | Once at init |
| `iroh_abi_version_patch` | Sync | Once at init |
| `iroh_last_error_message` | Sync | After errors |
| `iroh_status_name` | Sync | Diagnostic |
| `iroh_string_release` | Sync | After string reads |
| `iroh_operation_cancel` | Sync | Cancellation |

---

## Binding Coverage Matrix

Tracks which FFI functions each language binding exposes.

### Legend
- **Y** = Implemented and wired to real FFI
- **P** = Partial (struct exists but not all methods)
- **—** = Not implemented

### Transport (Tier 1-2)

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `iroh_runtime_new` | Y | Y | Y | Y |
| `iroh_runtime_close` | Y | Y | Y | Y |
| `iroh_poll_events` | Y | Y | Y | Y |
| `iroh_buffer_release` | Y | Y | Y | Y |
| `iroh_operation_cancel` | Y | Y | Y | Y |
| `iroh_node_memory` | Y | Y | Y | — |
| `iroh_node_memory_with_alpns` | Y | Y | Y | Y |
| `iroh_node_persistent` | Y | Y | Y | — |
| `iroh_node_id` | Y | Y | Y | Y |
| `iroh_node_addr_info` | Y | Y | Y | Y |
| `iroh_node_free` | Y | Y | Y | Y |
| `iroh_endpoint_create` | Y | Y | Y | Y |
| `iroh_endpoint_close` | Y | Y | Y | Y |
| `iroh_endpoint_id` | Y | Y | Y | Y |
| `iroh_connect` | Y | Y | Y | Y |
| `iroh_accept` | Y | Y | Y | Y |
| `iroh_open_bi` | Y | Y | Y | Y |
| `iroh_accept_bi` | Y | Y | Y | Y |
| `iroh_stream_write` | Y | Y | Y | Y |
| `iroh_stream_read` | Y | Y | Y | Y |
| `iroh_stream_finish` | Y | Y | Y | Y |
| `iroh_connection_close` | Y | Y | Y | Y |
| `iroh_connection_remote_id` | Y | Y | Y | Y |

### Reactor

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `aster_reactor_create` | — | Y | Y | Y |
| `aster_reactor_destroy` | — | Y | Y | Y |
| `aster_reactor_poll` | — | Y | Y | Y |
| `aster_reactor_submit` | — | Y | Y | Y |
| `aster_reactor_buffer_release` | — | Y | Y | Y |

Note: Python uses PyO3 direct Rust bindings, not the C FFI reactor.

### Blobs

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `iroh_blobs_add_bytes` | Y | Y | Y | Y |
| `iroh_blobs_read` | Y | Y | Y | Y |
| `iroh_blobs_download` | Y | Y | Y | Y |
| `iroh_blobs_status` | Y | Y | Y | — |
| `iroh_blobs_has` | Y | Y | Y | — |
| `iroh_blobs_create_ticket` | Y | Y | Y | — |
| `iroh_blobs_observe_complete` | Y | Y | Y | — |
| `iroh_blobs_add_bytes_as_collection` | Y | Y | Y | — |
| `iroh_blobs_add_collection` | Y | Y | Y | — |
| `iroh_blobs_list_collection` | Y | Y | Y | — |
| `iroh_blobs_create_collection_ticket` | Y | Y | — | — |
| `iroh_blobs_observe_snapshot` | Y | Y | — | — |
| `iroh_blobs_local_info` | Y | Y | Y | — |

### Docs

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `iroh_docs_create` | Y | Y | Y | Y |
| `iroh_docs_create_author` | Y | Y | Y | Y |
| `iroh_docs_join` | Y | Y | Y | Y |
| `iroh_doc_set_bytes` | Y | Y | Y | Y |
| `iroh_doc_get_exact` | Y | Y | Y | Y |
| `iroh_doc_query` | Y | Y | Y | — |
| `iroh_doc_share` | Y | Y | Y | Y |
| `iroh_doc_subscribe` | Y | — | — | — |
| `iroh_doc_event_recv` | Y | — | — | — |
| `iroh_doc_start_sync` | Y | Y | Y | — |
| `iroh_doc_leave` | Y | Y | Y | — |
| `iroh_doc_read_entry_content` | Y | Y | Y | — |
| `iroh_doc_free` | Y | Y | Y | Y |

### Gossip

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `iroh_gossip_subscribe` | Y | Y | Y | Y |
| `iroh_gossip_broadcast` | Y | Y | Y | Y |
| `iroh_gossip_recv` | Y | Y | Y | Y |
| `iroh_gossip_topic_free` | Y | Y | Y | Y |

### Tags

| Function | Python | Java | Go | .NET |
|----------|--------|------|----|------|
| `iroh_tags_set` | Y | P | Y | Y |
| `iroh_tags_get` | Y | P | Y | Y |
| `iroh_tags_delete` | Y | P | Y | Y |
| `iroh_tags_list_prefix` | Y | P | Y | — |

---

## Performance Optimization Scope

Based on this classification, FFI performance work should focus on:

### Phase 1 — Reactor hot path (8 functions)
The reactor poll/submit/release cycle and CQ drain. This is where the SPSC ring buffer, zero-copy, and batch amortization work lands.

### Phase 2 — Stream I/O (3 functions)
`read_into` from aster-rpc/noq eliminates per-read Bytes allocation. Benefits both reactor-internal stream handling and direct stream users.

### Phase 3 — Blob data path (2-3 functions)
If user apps do high-throughput file transfer, `iroh_blobs_add_bytes` and `iroh_blobs_read` may need the same zero-copy treatment.

### Not in scope for perf work
Everything else. Setup, lifecycle, diagnostics, and infrequent operations. These just need to work correctly.

---

## Updating This Document

When adding a new FFI function:
1. Add it to the appropriate path classification (hot/warm/cold)
2. Add it to the binding coverage matrix
3. If hot path, note the optimization target
