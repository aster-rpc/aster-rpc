# C ABI rename map (ABI 2.x -> 3.0) and 3.x changelog

ABI 3.0.0 renames the entire C ABI of `aster_transport_ffi` to the single
`aster_` / `ASTER_` prefix. No backward compatibility: a binding must switch
every symbol, pass `ASTER_ABI_VERSION_MAJOR` (3) to `aster_runtime_new`, and
should check `aster_abi_version() >> 16 == 3` at load.

Header: `ffi/aster_ffi.h` (was `ffi/iroh_ffi.h`). Library file name unchanged
(`libaster_transport_ffi.{so,dylib}`, `aster_transport_ffi.dll`).

## Signature / semantic changes (beyond the rename)

| Old | New | Change |
|---|---|---|
| `int32_t iroh_runtime_new(const iroh_runtime_config_t*, iroh_runtime_t*)` | `int32_t aster_runtime_new(uint32_t expected_abi_major, const aster_runtime_config_t*, aster_runtime_t*)` | New first argument. Mismatch returns `ASTER_STATUS_ABI_MISMATCH` (12) and sets the last-error message; nothing is created. A non-NULL config must have `struct_size == sizeof(aster_runtime_config_t)` (16), else `ASTER_STATUS_INVALID_ARGUMENT`. |
| `uint32_t iroh_abi_version_major/minor/patch(void)` | `uint32_t aster_abi_version(void)` | One packed value `(major << 16) \| (minor << 8) \| patch`. Macro `ASTER_ABI_VERSION` is the header's packed value. |
| `const char* iroh_status_name(iroh_status_t)` | `const char* aster_status_name(int32_t)` | Raw status int (no UB on unknown values); unknown -> `"ASTER_STATUS_UNKNOWN"`. |
| `int32_t iroh_hook_before_connect_respond(rt, inv, iroh_hook_decision_t)` | `int32_t aster_hook_before_connect_respond(rt, inv, uint32_t decision)` | Same width; values other than ALLOW/DENY -> `ASTER_STATUS_INVALID_ARGUMENT`. |
| `iroh_endpoint_config_t.relay_mode`: unknown value silently = default | `aster_relay_mode_t`; unknown -> `ASTER_STATUS_INVALID_ARGUMENT` | `ASTER_RELAY_MODE_STAGING` = 3 now named. |
| `iroh_doc_share` / `iroh_doc_share_with_addr` `mode`: 0 = read, anything else = write | `aster_doc_share_mode_t`: 0 read, 1 write, other -> `ASTER_STATUS_INVALID_ARGUMENT` | |
| `ASTER_EVENT_KIND_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` (reactor `event_kind`, uint8) | `ASTER_REACTOR_EVENT_CALL/CONNECTION_CLOSED/RESPONSE_WRITABLE/RESPONSE_CLOSED` | Values unchanged (0..3). Renamed so they cannot be confused with the completion-queue `ASTER_EVENT_*` values of `aster_event_kind_t`. |

## New in 3.2 (additive)

ABI 3.2.0 puts the C ABI on the `aster` crate (plan D1, tranche F1): nodes start
from an `aster::AsterConfig` through `aster::Node::start`, and Gate 0 /
admission decisions come from the Rust `GatePolicy`. Nothing in 3.0/3.1 changed
shape; a 3.1 binding keeps working. Source: `ffi/src/node.rs`; tests:
`ffi/src/node.rs` (config round-trip / env / TOML), `ffi/tests/node_start.rs`,
`ffi/tests/abi_contract_test.c`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_config_t` (280 B) | `struct_size, relay_mode, relay_urls, secret_key (32 raw bytes or empty), secret_key_file, storage_path, gc_interval_ms, bind_addr, alpns, identity_file, proxy_url, hook_timeout_ms (0 = 5000), hook_failure_mode, enable_hooks, enable_monitoring, local_discovery, clear_ip_transports, clear_relay_transports, portmapper, proxy_from_env, transport_set, transport_initial_mtu, transport_send_fairness, transport_enable_segmentation_offload, transport_max_concurrent_{bidi,uni}_streams, transport_{stream_receive,receive,send}_window, transport_max_idle_timeout_ms, transport_keep_alive_interval_ms, transport_datagram_{receive,send}_buffer_size, storage` |
| enum | `aster_portmapper_t` | `DEFAULT`=0 (unset), `ENABLED`=1, `DISABLED`=2 |
| enum | `aster_trust_mode_t` | `PROTECTED`=0 (default), `OPEN_DEV`=1 |
| macro | `ASTER_TRANSPORT_*` (bits 0..11), `ASTER_TRANSPORT_ALL` | presence bits of `aster_config_t.transport_set`; unknown bits rejected |
| type | `aster_gate_t` | `uint64_t` handle, runtime-scoped |
| function | `aster_config_init(aster_config_t*)` | Rust defaults; never allocates (`storage == NULL`) |
| function | `aster_config_from_env(aster_config_t*)` | `AsterConfig::from_env`; release with `aster_config_release` |
| function | `aster_config_from_toml(aster_bytes_t path, aster_config_t*)` | `AsterConfig::from_file` (file, then `ASTER_*` env); release with `aster_config_release` |
| function | `aster_config_release(aster_config_t*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_node_start(rt, const aster_config_t*, aster_gate_t gate, user_data, out_op)` | async → `ASTER_EVENT_NODE_CREATED` / `ASTER_EVENT_ERROR`; config validated + copied before return (bad config → `INVALID_ARGUMENT`, nothing started); `gate` ≠ 0 forces hooks on |
| function | `aster_node_set_gate(rt, node, gate)` | attach/detach (0); `UNSUPPORTED` for nodes without hooks |
| function | `aster_node_metrics_prometheus(rt, node, aster_bytes_t* out)` | free with `aster_string_release` |
| function | `aster_gate_new`, `aster_gate_free`, `aster_gate_set_trust_mode`, `aster_gate_set_admission_alpns`, `aster_gate_set_reject(code, reason)`, `aster_gate_admit`, `aster_gate_revoke`, `aster_gate_is_admitted`, `aster_gate_should_allow` | peers are 64-hex node ids (case-insensitive) |
| function | `aster_admission_respond(rt, invocation, decision, reject_code, reason)` | answers `ASTER_EVENT_HOOK_{BEFORE,AFTER}_CONNECT` from nodes and endpoints; DENY of an inbound invocation closes with `reject_code`/`reason` |

Admission events on nodes: a node started with `enable_hooks` and no gate raises
the same `ASTER_EVENT_HOOK_BEFORE_CONNECT` / `ASTER_EVENT_HOOK_AFTER_CONNECT`
events as an endpoint (`event.handle` = node, `event.related` = invocation,
`event.user_data` = the start call's `user_data`, same payload encoding). With a
gate attached, inbound peers are decided in Rust (admitted / admission ALPN /
`OPEN_DEV` → accept, else close with the gate's code + reason) and outbound
dials are allowed, without events.

Behaviour fixes in 3.2 (no signature change): `aster_node_addr_info` now fills
`direct_addresses` (it always returned an empty list); `aster_endpoint_addr_info`
returns `INVALID_ARGUMENT` for a NULL scratch buffer. Handle ids are now unique across
all handle kinds within the process (one counter), so a function taking "endpoint
or node" can no longer confuse an endpoint with a node of the same number.

**Deprecated (removal in 4.0):** `aster_node_memory`, `aster_node_memory_with_alpns`,
`aster_node_persistent`, `aster_endpoint_create` (and with it the
endpoint-only `aster_endpoint_*` functions and `aster_endpoint_config_t`). Use
`aster_node_start`; `aster_connect`, `aster_accept`, remote info and transport
metrics already take node handles.

## New in 3.1 (additive)

ABI 3.1.0 adds the AEv1 RPC envelope codec (Aster-SPEC §6.9) and zstd payload
compression (§5.6) from `aster_transport_core`, so FFI bindings stop encoding
the envelope with their own Fory. Nothing in 3.0 changed; a 3.0 binding keeps
working against a 3.1 library. Source: `ffi/src/envelope.rs`; vectors checked
by `ffi/tests/envelope_vectors.rs`.

| Kind | Name | Notes |
|---|---|---|
| struct | `aster_stream_header_t` (96 B) | `struct_size, version, call_id, session_id, deadline:i16, serialization_mode:i8, reserved0:u8, reserved1:u32, service, method, metadata_keys, metadata_values, storage` |
| struct | `aster_call_header_t` (72 B) | `struct_size, call_id, deadline:i16, reserved0:u16, reserved1:u32, method, metadata_keys, metadata_values, storage` |
| struct | `aster_rpc_status_t` (64 B) | `struct_size, code, message, detail_keys, detail_values, storage` |
| function | `aster_{stream_header,call_header,rpc_status}_encode(const T*, aster_bytes_t* out)` | caller sets `struct_size`; `out` freed with `aster_string_release` |
| function | `aster_{stream_header,call_header,rpc_status}_decode(const uint8_t*, size_t, T* out)` | library writes `struct_size` and `storage`; strings/lists point into `storage`; empty = `{NULL,0}`; not NUL-terminated |
| function | `aster_{stream_header,call_header,rpc_status}_release(T*)` | frees `storage`, zeroes pointers; no-op when `storage == NULL` |
| function | `aster_zstd_maybe_compress(data, len, threshold, aster_bytes_t* out)` | `out == {NULL,0}` means send uncompressed |
| function | `aster_zstd_decompress(data, len, aster_bytes_t* out)` | 16 MiB cap; corrupt / over-limit -> `ASTER_STATUS_INVALID_ARGUMENT` |
| macro | `ASTER_COMPRESSION_THRESHOLD` (4096), `ASTER_MAX_DECOMPRESSED_SIZE` (16 MiB), `ASTER_ENVELOPE_TAG_{STREAM_HEADER,CALL_HEADER,RPC_STATUS}` (0xA1..0xA3), `ASTER_ENVELOPE_VERSION` (1) | |

Errors: malformed / non-canonical input, wrong `struct_size`, non-UTF-8
strings and key/value length mismatches all return
`ASTER_STATUS_INVALID_ARGUMENT` with `aster_last_error_message` set.

## New in 3.0

| Kind | Name | Notes |
|---|---|---|
| function | `aster_abi_version` | packed version |
| macro | `ASTER_ABI_VERSION` | packed version of the header |
| enum-value | `ASTER_STATUS_ABI_MISMATCH` = 12 | |
| enum-value | `ASTER_RELAY_MODE_STAGING` = 3 | |
| type | `aster_connection_type_t`: `ASTER_CONNECTION_TYPE_NOT_CONNECTED`=0, `ASTER_CONNECTION_TYPE_CONNECTING`=1, `ASTER_CONNECTION_TYPE_UDP_DIRECT`=2, `ASTER_CONNECTION_TYPE_UDP_RELAY`=3, `ASTER_CONNECTION_TYPE_OTHER`=4 | `connection_type` of `aster_remote_info_t` / `aster_connection_info_t` |
| type | `aster_path_kind_t`: `ASTER_PATH_KIND_NONE`=0, `ASTER_PATH_KIND_DIRECT`=1, `ASTER_PATH_KIND_RELAY`=2 | `aster_transport_snapshot_t.path_kind` |
| type | `aster_doc_share_mode_t`: `ASTER_DOC_SHARE_READ`=0, `ASTER_DOC_SHARE_WRITE`=1 | `mode` of `aster_doc_share*` |
| type | `aster_registry_key_kind_t`: `ASTER_REGISTRY_KEY_CONTRACT`=0, `_VERSION`=1, `_CHANNEL`=2, `_LEASE`=3, `_LEASE_PREFIX`=4, `_ACL`=5 | `kind` of `aster_registry_key` |

All ABI enums are now emitted in the header (previously `iroh_event_kind_t`,
`iroh_relay_mode_t`, `iroh_doc_query_mode_t`, `iroh_download_policy_mode_t`
were missing): `aster_status_t`, `aster_event_kind_t`, `aster_relay_mode_t`,
`aster_hook_failure_mode_t`, `aster_hook_decision_t`, `aster_connection_type_t`,
`aster_path_kind_t`, `aster_doc_query_mode_t`, `aster_download_policy_mode_t`,
`aster_doc_share_mode_t`, `aster_registry_key_kind_t`. Struct fields and
arguments still carry them as `uint32_t` (C enum width is not ABI-stable
across compilers and an out-of-range value would be UB in Rust).

Struct layouts are **unchanged** (pinned in `ffi/src/layout.rs` and
`ffi/tests/abi_contract_test.c`; LP64): `aster_event_t` 80,
`aster_runtime_config_t` 16, `aster_endpoint_config_t` 152,
`aster_connect_config_t` 48, `aster_node_addr_t` 48, `aster_bytes_t` 16,
`aster_bytes_list_t` 16, `aster_remote_info_t` 72, `aster_connection_info_t` 56,
`aster_transport_snapshot_t` 56, `aster_transport_metrics_t` 136,
`aster_reactor_call_t` 112.

Symbols that were already `aster_*` / `ASTER_*` (reactor, call pool,
contract, ticket, registry, canonical/signing, `ASTER_CALL_*`,
`ASTER_RECV_FRAME_*`) are unchanged, except the `ASTER_EVENT_KIND_*` row above.

## Full rename table

One row per symbol; columns `kind | old | new | note`. Mechanical rule:
`iroh_` -> `aster_`, `IROH_` -> `ASTER_`, plus the rows below the first rule.

| kind | old | new | note |
|---|---|---|---|
| macro | `ASTER_EVENT_KIND_CALL` | `ASTER_REACTOR_EVENT_CALL` | reactor rename |
| macro | `ASTER_EVENT_KIND_CONNECTION_CLOSED` | `ASTER_REACTOR_EVENT_CONNECTION_CLOSED` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_WRITABLE` | `ASTER_REACTOR_EVENT_RESPONSE_WRITABLE` | reactor rename |
| macro | `ASTER_EVENT_KIND_RESPONSE_CLOSED` | `ASTER_REACTOR_EVENT_RESPONSE_CLOSED` | reactor rename |
| function | `iroh_accept` | `aster_accept` |  |
| function | `iroh_accept_bi` | `aster_accept_bi` |  |
| function | `iroh_accept_uni` | `aster_accept_uni` |  |
| function | `iroh_add_node_addr` | `aster_add_node_addr` |  |
| function | `iroh_blobs_add_bytes` | `aster_blobs_add_bytes` |  |
| function | `iroh_blobs_add_bytes_as_collection` | `aster_blobs_add_bytes_as_collection` |  |
| function | `iroh_blobs_add_collection` | `aster_blobs_add_collection` |  |
| function | `iroh_blobs_add_path` | `aster_blobs_add_path` |  |
| function | `iroh_blobs_add_path_with_named_tag` | `aster_blobs_add_path_with_named_tag` |  |
| function | `iroh_blobs_create_collection_ticket` | `aster_blobs_create_collection_ticket` |  |
| function | `iroh_blobs_create_ticket` | `aster_blobs_create_ticket` |  |
| function | `iroh_blobs_download` | `aster_blobs_download` |  |
| function | `iroh_blobs_has` | `aster_blobs_has` |  |
| function | `iroh_blobs_list_collection` | `aster_blobs_list_collection` |  |
| function | `iroh_blobs_local_info` | `aster_blobs_local_info` |  |
| function | `iroh_blobs_observe_complete` | `aster_blobs_observe_complete` |  |
| function | `iroh_blobs_observe_snapshot` | `aster_blobs_observe_snapshot` |  |
| function | `iroh_blobs_read` | `aster_blobs_read` |  |
| function | `iroh_blobs_status` | `aster_blobs_status` |  |
| function | `iroh_buffer_release` | `aster_buffer_release` |  |
| function | `iroh_connect` | `aster_connect` |  |
| function | `iroh_connection_authorize_tunnel_tcp` | `aster_connection_authorize_tunnel_tcp` |  |
| function | `iroh_connection_close` | `aster_connection_close` |  |
| function | `iroh_connection_closed` | `aster_connection_closed` |  |
| function | `iroh_connection_datagram_send_buffer_space` | `aster_connection_datagram_send_buffer_space` |  |
| function | `iroh_connection_free` | `aster_connection_free` |  |
| function | `iroh_connection_info` | `aster_connection_info` |  |
| function | `iroh_connection_max_datagram_size` | `aster_connection_max_datagram_size` |  |
| function | `iroh_connection_open_tunnel` | `aster_connection_open_tunnel` |  |
| function | `iroh_connection_read_datagram` | `aster_connection_read_datagram` |  |
| function | `iroh_connection_remote_id` | `aster_connection_remote_id` |  |
| function | `iroh_connection_send_datagram` | `aster_connection_send_datagram` |  |
| function | `iroh_connection_transport_snapshot` | `aster_connection_transport_snapshot` |  |
| function | `iroh_doc_del` | `aster_doc_del` |  |
| function | `iroh_doc_event_recv` | `aster_doc_event_recv` |  |
| function | `iroh_doc_free` | `aster_doc_free` |  |
| function | `iroh_doc_get_exact` | `aster_doc_get_exact` |  |
| function | `iroh_doc_id` | `aster_doc_id` |  |
| function | `iroh_doc_leave` | `aster_doc_leave` |  |
| function | `iroh_doc_query` | `aster_doc_query` |  |
| function | `iroh_doc_read_entry_content` | `aster_doc_read_entry_content` |  |
| function | `iroh_doc_set_bytes` | `aster_doc_set_bytes` |  |
| function | `iroh_doc_set_download_policy` | `aster_doc_set_download_policy` |  |
| function | `iroh_doc_share` | `aster_doc_share` |  |
| function | `iroh_doc_share_with_addr` | `aster_doc_share_with_addr` |  |
| function | `iroh_doc_start_sync` | `aster_doc_start_sync` |  |
| function | `iroh_doc_subscribe` | `aster_doc_subscribe` |  |
| function | `iroh_docs_create` | `aster_docs_create` |  |
| function | `iroh_docs_create_author` | `aster_docs_create_author` |  |
| function | `iroh_docs_join` | `aster_docs_join` |  |
| function | `iroh_docs_join_and_subscribe` | `aster_docs_join_and_subscribe` |  |
| function | `iroh_endpoint_addr_info` | `aster_endpoint_addr_info` |  |
| function | `iroh_endpoint_close` | `aster_endpoint_close` |  |
| function | `iroh_endpoint_create` | `aster_endpoint_create` |  |
| function | `iroh_endpoint_export_secret_key` | `aster_endpoint_export_secret_key` |  |
| function | `iroh_endpoint_free` | `aster_endpoint_free` |  |
| function | `iroh_endpoint_id` | `aster_endpoint_id` |  |
| function | `iroh_endpoint_remote_info` | `aster_endpoint_remote_info` |  |
| function | `iroh_endpoint_remote_info_list` | `aster_endpoint_remote_info_list` |  |
| function | `iroh_endpoint_transport_metrics` | `aster_endpoint_transport_metrics` |  |
| function | `IROH_EVENT_BLOB_OBSERVE_COMPLETE` | `ASTER_EVENT_BLOB_OBSERVE_COMPLETE` |  |
| function | `IROH_EVENT_DOC_SUBSCRIBED` | `ASTER_EVENT_DOC_SUBSCRIBED` |  |
| function | `iroh_gossip_broadcast` | `aster_gossip_broadcast` |  |
| function | `iroh_gossip_recv` | `aster_gossip_recv` |  |
| function | `iroh_gossip_subscribe` | `aster_gossip_subscribe` |  |
| function | `iroh_gossip_topic_free` | `aster_gossip_topic_free` |  |
| function | `iroh_hook_after_connect_respond` | `aster_hook_after_connect_respond` |  |
| function | `iroh_hook_before_connect_respond` | `aster_hook_before_connect_respond` |  |
| function | `IROH_HOOK_DECISION_ALLOW` | `ASTER_HOOK_DECISION_ALLOW` |  |
| function | `IROH_HOOK_DECISION_DENY` | `ASTER_HOOK_DECISION_DENY` |  |
| function | `iroh_last_error_message` | `aster_last_error_message` |  |
| function | `iroh_node_accept_aster` | `aster_node_accept_aster` |  |
| function | `iroh_node_addr_info` | `aster_node_addr_info` |  |
| function | `iroh_node_close` | `aster_node_close` |  |
| function | `iroh_node_export_secret_key` | `aster_node_export_secret_key` |  |
| function | `iroh_node_free` | `aster_node_free` |  |
| function | `iroh_node_id` | `aster_node_id` |  |
| function | `iroh_node_memory` | `aster_node_memory` |  |
| function | `iroh_node_memory_with_alpns` | `aster_node_memory_with_alpns` |  |
| function | `iroh_node_persistent` | `aster_node_persistent` |  |
| function | `iroh_open_bi` | `aster_open_bi` |  |
| function | `iroh_open_uni` | `aster_open_uni` |  |
| function | `iroh_operation_cancel` | `aster_operation_cancel` |  |
| function | `iroh_poll_events` | `aster_poll_events` |  |
| function | `iroh_recv_stream_free` | `aster_recv_stream_free` |  |
| function | `iroh_runtime_close` | `aster_runtime_close` |  |
| function | `iroh_runtime_new` | `aster_runtime_new` |  |
| function | `iroh_secret_key_generate` | `aster_secret_key_generate` |  |
| function | `iroh_send_stream_free` | `aster_send_stream_free` |  |
| function | `iroh_status_name` | `aster_status_name` |  |
| function | `IROH_STATUS_NOT_FOUND` | `ASTER_STATUS_NOT_FOUND` |  |
| function | `iroh_stream_finish` | `aster_stream_finish` |  |
| function | `iroh_stream_read` | `aster_stream_read` |  |
| function | `iroh_stream_read_exact` | `aster_stream_read_exact` |  |
| function | `iroh_stream_read_to_end` | `aster_stream_read_to_end` |  |
| function | `iroh_stream_stop` | `aster_stream_stop` |  |
| function | `iroh_stream_stopped` | `aster_stream_stopped` |  |
| function | `iroh_stream_write` | `aster_stream_write` |  |
| function | `iroh_string_release` | `aster_string_release` |  |
| function | `iroh_tags_delete` | `aster_tags_delete` |  |
| function | `iroh_tags_get` | `aster_tags_get` |  |
| function | `iroh_tags_list_prefix` | `aster_tags_list_prefix` |  |
| function | `iroh_tags_set` | `aster_tags_set` |  |
| type | `iroh_buffer_t` | `aster_buffer_t` |  |
| type | `iroh_bytes_list_t` | `aster_bytes_list_t` |  |
| type | `iroh_bytes_t` | `aster_bytes_t` |  |
| type | `iroh_connect_config_t` | `aster_connect_config_t` |  |
| type | `iroh_connection_info_t` | `aster_connection_info_t` |  |
| type | `iroh_connection_t` | `aster_connection_t` |  |
| type | `iroh_doc_query_mode_t` | `aster_doc_query_mode_t` | was missing from the old header |
| type | `iroh_download_policy_mode_t` | `aster_download_policy_mode_t` | was missing from the old header |
| type | `iroh_endpoint_config_t` | `aster_endpoint_config_t` |  |
| type | `iroh_endpoint_t` | `aster_endpoint_t` |  |
| type | `iroh_event_kind_t` | `aster_event_kind_t` | was missing from the old header |
| type | `iroh_event_t` | `aster_event_t` |  |
| type | `iroh_hook_decision_t` | `aster_hook_decision_t` |  |
| type | `iroh_hook_failure_mode_t` | `aster_hook_failure_mode_t` |  |
| type | `iroh_hook_invocation_t` | `aster_hook_invocation_t` |  |
| type | `iroh_node_addr_t` | `aster_node_addr_t` |  |
| type | `iroh_node_t` | `aster_node_t` |  |
| type | `iroh_operation_t` | `aster_operation_t` |  |
| type | `iroh_recv_stream_t` | `aster_recv_stream_t` |  |
| type | `iroh_relay_mode_t` | `aster_relay_mode_t` | was missing from the old header |
| type | `iroh_remote_info_t` | `aster_remote_info_t` |  |
| type | `iroh_runtime_config_t` | `aster_runtime_config_t` |  |
| type | `iroh_runtime_t` | `aster_runtime_t` |  |
| type | `iroh_send_stream_t` | `aster_send_stream_t` |  |
| type | `iroh_status_t` | `aster_status_t` |  |
| type | `iroh_transport_metrics_t` | `aster_transport_metrics_t` |  |
| type | `iroh_transport_snapshot_t` | `aster_transport_snapshot_t` |  |
| macro | `IROH_ABI_VERSION_MAJOR` | `ASTER_ABI_VERSION_MAJOR` |  |
| macro | `IROH_ABI_VERSION_MINOR` | `ASTER_ABI_VERSION_MINOR` |  |
| macro | `IROH_ABI_VERSION_PATCH` | `ASTER_ABI_VERSION_PATCH` |  |
| enum-value | `IROH_DOC_QUERY_KEY_EXACT` | `ASTER_DOC_QUERY_KEY_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_KEY_PREFIX` | `ASTER_DOC_QUERY_KEY_PREFIX` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_EXACT` | `ASTER_DOC_QUERY_LATEST_EXACT` |  |
| enum-value | `IROH_DOC_QUERY_LATEST_PREFIX` | `ASTER_DOC_QUERY_LATEST_PREFIX` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING` | `ASTER_DOWNLOAD_POLICY_EVERYTHING` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_DOWNLOAD_POLICY_NOTHING_EXCEPT` | `ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT` | was missing from the old header |
| enum-value | `IROH_EVENT_ASTER_ACCEPTED` | `ASTER_EVENT_ASTER_ACCEPTED` |  |
| enum-value | `IROH_EVENT_AUTHOR_CREATED` | `ASTER_EVENT_AUTHOR_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_ADDED` | `ASTER_EVENT_BLOB_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_ADDED` | `ASTER_EVENT_BLOB_COLLECTION_ADDED` |  |
| enum-value | `IROH_EVENT_BLOB_COLLECTION_TICKET_CREATED` | `ASTER_EVENT_BLOB_COLLECTION_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_DOWNLOADED` | `ASTER_EVENT_BLOB_DOWNLOADED` | was missing from the old header |
| enum-value | `IROH_EVENT_BLOB_READ` | `ASTER_EVENT_BLOB_READ` |  |
| enum-value | `IROH_EVENT_BLOB_TICKET_CREATED` | `ASTER_EVENT_BLOB_TICKET_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_BYTES_RESULT` | `ASTER_EVENT_BYTES_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_CLOSED` | `ASTER_EVENT_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECT_FAILED` | `ASTER_EVENT_CONNECT_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTED` | `ASTER_EVENT_CONNECTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_ACCEPTED` | `ASTER_EVENT_CONNECTION_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_CONNECTION_CLOSED` | `ASTER_EVENT_CONNECTION_CLOSED` | was missing from the old header |
| enum-value | `IROH_EVENT_DATAGRAM_RECEIVED` | `ASTER_EVENT_DATAGRAM_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_CREATED` | `ASTER_EVENT_DOC_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_DEL` | `ASTER_EVENT_DOC_DEL` |  |
| enum-value | `IROH_EVENT_DOC_EVENT` | `ASTER_EVENT_DOC_EVENT` |  |
| enum-value | `IROH_EVENT_DOC_GET` | `ASTER_EVENT_DOC_GET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED` | `ASTER_EVENT_DOC_JOINED` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_JOINED_AND_SUBSCRIBED` | `ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED` |  |
| enum-value | `IROH_EVENT_DOC_QUERY` | `ASTER_EVENT_DOC_QUERY` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SET` | `ASTER_EVENT_DOC_SET` | was missing from the old header |
| enum-value | `IROH_EVENT_DOC_SHARED` | `ASTER_EVENT_DOC_SHARED` |  |
| enum-value | `IROH_EVENT_ENDPOINT_CREATE_FAILED` | `ASTER_EVENT_ENDPOINT_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_ENDPOINT_CREATED` | `ASTER_EVENT_ENDPOINT_CREATED` | was missing from the old header |
| enum-value | `IROH_EVENT_ERROR` | `ASTER_EVENT_ERROR` | was missing from the old header |
| enum-value | `IROH_EVENT_FRAME_RECEIVED` | `ASTER_EVENT_FRAME_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_BROADCAST_DONE` | `ASTER_EVENT_GOSSIP_BROADCAST_DONE` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_LAGGED` | `ASTER_EVENT_GOSSIP_LAGGED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_DOWN` | `ASTER_EVENT_GOSSIP_NEIGHBOR_DOWN` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_NEIGHBOR_UP` | `ASTER_EVENT_GOSSIP_NEIGHBOR_UP` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_RECEIVED` | `ASTER_EVENT_GOSSIP_RECEIVED` | was missing from the old header |
| enum-value | `IROH_EVENT_GOSSIP_SUBSCRIBED` | `ASTER_EVENT_GOSSIP_SUBSCRIBED` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_AFTER_CONNECT` | `ASTER_EVENT_HOOK_AFTER_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_BEFORE_CONNECT` | `ASTER_EVENT_HOOK_BEFORE_CONNECT` | was missing from the old header |
| enum-value | `IROH_EVENT_HOOK_INVOCATION_RELEASED` | `ASTER_EVENT_HOOK_INVOCATION_RELEASED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATE_FAILED` | `ASTER_EVENT_NODE_CREATE_FAILED` | was missing from the old header |
| enum-value | `IROH_EVENT_NODE_CREATED` | `ASTER_EVENT_NODE_CREATED` |  |
| enum-value | `IROH_EVENT_NONE` | `ASTER_EVENT_NONE` | was missing from the old header |
| enum-value | `IROH_EVENT_OPERATION_CANCELLED` | `ASTER_EVENT_OPERATION_CANCELLED` | was missing from the old header |
| enum-value | `IROH_EVENT_REGISTRY_ACL_LISTED` | `ASTER_EVENT_REGISTRY_ACL_LISTED` |  |
| enum-value | `IROH_EVENT_REGISTRY_ACL_UPDATED` | `ASTER_EVENT_REGISTRY_ACL_UPDATED` |  |
| enum-value | `IROH_EVENT_REGISTRY_PUBLISHED` | `ASTER_EVENT_REGISTRY_PUBLISHED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RENEWED` | `ASTER_EVENT_REGISTRY_RENEWED` |  |
| enum-value | `IROH_EVENT_REGISTRY_RESOLVED` | `ASTER_EVENT_REGISTRY_RESOLVED` |  |
| enum-value | `IROH_EVENT_SEND_COMPLETED` | `ASTER_EVENT_SEND_COMPLETED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_ACCEPTED` | `ASTER_EVENT_STREAM_ACCEPTED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_FINISHED` | `ASTER_EVENT_STREAM_FINISHED` | was missing from the old header |
| enum-value | `IROH_EVENT_STREAM_OPENED` | `ASTER_EVENT_STREAM_OPENED` |  |
| enum-value | `IROH_EVENT_STREAM_RESET` | `ASTER_EVENT_STREAM_RESET` | was missing from the old header |
| enum-value | `IROH_EVENT_STRING_RESULT` | `ASTER_EVENT_STRING_RESULT` | was missing from the old header |
| enum-value | `IROH_EVENT_TAG_DELETED` | `ASTER_EVENT_TAG_DELETED` |  |
| enum-value | `IROH_EVENT_TAG_GET` | `ASTER_EVENT_TAG_GET` |  |
| enum-value | `IROH_EVENT_TAG_LIST` | `ASTER_EVENT_TAG_LIST` |  |
| enum-value | `IROH_EVENT_TAG_SET` | `ASTER_EVENT_TAG_SET` |  |
| enum-value | `IROH_EVENT_UNIT_RESULT` | `ASTER_EVENT_UNIT_RESULT` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_CLOSED` | `ASTER_HOOK_FAILURE_FAIL_CLOSED` |  |
| enum-value | `IROH_HOOK_FAILURE_FAIL_OPEN` | `ASTER_HOOK_FAILURE_FAIL_OPEN` |  |
| enum-value | `IROH_RELAY_MODE_CUSTOM` | `ASTER_RELAY_MODE_CUSTOM` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DEFAULT` | `ASTER_RELAY_MODE_DEFAULT` | was missing from the old header |
| enum-value | `IROH_RELAY_MODE_DISABLED` | `ASTER_RELAY_MODE_DISABLED` | was missing from the old header |
| enum-value | `IROH_STATUS_ALREADY_CLOSED` | `ASTER_STATUS_ALREADY_CLOSED` |  |
| enum-value | `IROH_STATUS_BUFFER_TOO_SMALL` | `ASTER_STATUS_BUFFER_TOO_SMALL` |  |
| enum-value | `IROH_STATUS_CANCELLED` | `ASTER_STATUS_CANCELLED` |  |
| enum-value | `IROH_STATUS_CONNECTION_REFUSED` | `ASTER_STATUS_CONNECTION_REFUSED` |  |
| enum-value | `IROH_STATUS_INTERNAL` | `ASTER_STATUS_INTERNAL` |  |
| enum-value | `IROH_STATUS_INVALID_ARGUMENT` | `ASTER_STATUS_INVALID_ARGUMENT` |  |
| enum-value | `IROH_STATUS_OK` | `ASTER_STATUS_OK` |  |
| enum-value | `IROH_STATUS_QUEUE_FULL` | `ASTER_STATUS_QUEUE_FULL` |  |
| enum-value | `IROH_STATUS_STREAM_RESET` | `ASTER_STATUS_STREAM_RESET` |  |
| enum-value | `IROH_STATUS_TIMEOUT` | `ASTER_STATUS_TIMEOUT` |  |
| enum-value | `IROH_STATUS_UNSUPPORTED` | `ASTER_STATUS_UNSUPPORTED` |  |
| function | `iroh_abi_version_major` | `aster_abi_version` | REMOVED: use `aster_abi_version() >> 16` |
| function | `iroh_abi_version_minor` | `aster_abi_version` | REMOVED: use `(aster_abi_version() >> 8) & 0xff` |
| function | `iroh_abi_version_patch` | `aster_abi_version` | REMOVED: use `aster_abi_version() & 0xff` |

Not listed: `iroh_buffer_free`, `iroh_doc_create`, `iroh_doc_join` appeared only in stale doc comments of the old header (no such functions; the real ones are `aster_string_release`, `aster_docs_create`, `aster_docs_join`).
