/*
 * C ABI contract test for aster_transport_ffi.
 *
 * Includes the generated header (ffi/aster_ffi.h) and checks, from the C
 * side, the same layout numbers that ffi/src/layout.rs asserts on the Rust
 * side. Layout is checked at compile time (_Static_assert) so a header/Rust
 * drift fails the build; ABI version, enum values and a few ownership rules
 * are checked at run time against the built library.
 *
 * Run with: ffi/scripts/run-c-abi-test.sh
 * (builds the cdylib, compiles this file against it, runs it).
 */

#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "aster_ffi.h"

#if UINTPTR_MAX != 0xFFFFFFFFFFFFFFFFu
#error "abi_contract_test.c assumes a 64-bit target"
#endif

#define LAYOUT(ty, size)                                                       \
    _Static_assert(sizeof(ty) == (size), "sizeof(" #ty ")")
#define OFF(ty, field, off)                                                    \
    _Static_assert(offsetof(ty, field) == (off), "offsetof(" #ty "." #field ")")

/* ── Layout (must match ffi/src/layout.rs) ─────────────────────────────── */

_Static_assert(sizeof(aster_status_t) == 4, "enum size");
_Static_assert(sizeof(aster_event_kind_t) == 4, "enum size");

LAYOUT(aster_bytes_t, 16);
OFF(aster_bytes_t, ptr, 0);
OFF(aster_bytes_t, len, 8);

LAYOUT(aster_bytes_list_t, 16);
OFF(aster_bytes_list_t, items, 0);
OFF(aster_bytes_list_t, len, 8);

LAYOUT(aster_runtime_config_t, 16);
OFF(aster_runtime_config_t, struct_size, 0);
OFF(aster_runtime_config_t, worker_threads, 4);
OFF(aster_runtime_config_t, event_queue_capacity, 8);
OFF(aster_runtime_config_t, reserved, 12);

LAYOUT(aster_endpoint_config_t, 152);
OFF(aster_endpoint_config_t, struct_size, 0);
OFF(aster_endpoint_config_t, relay_mode, 4);
OFF(aster_endpoint_config_t, secret_key, 8);
OFF(aster_endpoint_config_t, alpns, 24);
OFF(aster_endpoint_config_t, relay_urls, 40);
OFF(aster_endpoint_config_t, enable_discovery, 56);
OFF(aster_endpoint_config_t, enable_hooks, 60);
OFF(aster_endpoint_config_t, hook_timeout_ms, 64);
OFF(aster_endpoint_config_t, bind_addr, 72);
OFF(aster_endpoint_config_t, clear_ip_transports, 88);
OFF(aster_endpoint_config_t, clear_relay_transports, 92);
OFF(aster_endpoint_config_t, portmapper_config, 96);
OFF(aster_endpoint_config_t, proxy_url, 104);
OFF(aster_endpoint_config_t, proxy_from_env, 120);
OFF(aster_endpoint_config_t, data_dir_utf8, 128);
OFF(aster_endpoint_config_t, hook_failure_mode, 144);

LAYOUT(aster_connect_config_t, 48);
OFF(aster_connect_config_t, struct_size, 0);
OFF(aster_connect_config_t, flags, 4);
OFF(aster_connect_config_t, node_id, 8);
OFF(aster_connect_config_t, alpn, 24);
OFF(aster_connect_config_t, addr, 40);

LAYOUT(aster_node_addr_t, 48);
OFF(aster_node_addr_t, endpoint_id, 0);
OFF(aster_node_addr_t, relay_url, 16);
OFF(aster_node_addr_t, direct_addresses, 32);

LAYOUT(aster_event_t, 80);
OFF(aster_event_t, struct_size, 0);
OFF(aster_event_t, kind, 4);
OFF(aster_event_t, status, 8);
OFF(aster_event_t, operation, 16);
OFF(aster_event_t, handle, 24);
OFF(aster_event_t, related, 32);
OFF(aster_event_t, user_data, 40);
OFF(aster_event_t, data_ptr, 48);
OFF(aster_event_t, data_len, 56);
OFF(aster_event_t, buffer, 64);
OFF(aster_event_t, error_code, 72);
OFF(aster_event_t, flags, 76);

LAYOUT(aster_remote_info_t, 72);
OFF(aster_remote_info_t, struct_size, 0);
OFF(aster_remote_info_t, node_id, 8);
OFF(aster_remote_info_t, is_connected, 24);
OFF(aster_remote_info_t, connection_type, 28);
OFF(aster_remote_info_t, relay_url, 32);
OFF(aster_remote_info_t, last_handshake_ns, 48);
OFF(aster_remote_info_t, bytes_sent, 56);
OFF(aster_remote_info_t, bytes_received, 64);

LAYOUT(aster_connection_info_t, 56);
OFF(aster_connection_info_t, struct_size, 0);
OFF(aster_connection_info_t, connection_type, 4);
OFF(aster_connection_info_t, bytes_sent, 8);
OFF(aster_connection_info_t, bytes_received, 16);
OFF(aster_connection_info_t, rtt_ns, 24);
OFF(aster_connection_info_t, alpn, 32);
OFF(aster_connection_info_t, is_connected, 48);

LAYOUT(aster_transport_snapshot_t, 56);
OFF(aster_transport_snapshot_t, struct_size, 0);
OFF(aster_transport_snapshot_t, path_kind, 4);
OFF(aster_transport_snapshot_t, peer_host, 8);
OFF(aster_transport_snapshot_t, peer_port, 24);
OFF(aster_transport_snapshot_t, relay_url, 32);
OFF(aster_transport_snapshot_t, rtt_micros, 48);

LAYOUT(aster_transport_metrics_t, 136);
OFF(aster_transport_metrics_t, struct_size, 0);
OFF(aster_transport_metrics_t, send_ipv4, 8);
OFF(aster_transport_metrics_t, net_reports_full, 128);

LAYOUT(aster_reactor_call_t, 112);
OFF(aster_reactor_call_t, event_kind, 0);
OFF(aster_reactor_call_t, call_id, 8);
OFF(aster_reactor_call_t, connection_id, 16);
OFF(aster_reactor_call_t, stream_id, 24);
OFF(aster_reactor_call_t, header_ptr, 32);
OFF(aster_reactor_call_t, header_len, 40);
OFF(aster_reactor_call_t, header_flags, 44);
OFF(aster_reactor_call_t, request_ptr, 48);
OFF(aster_reactor_call_t, request_len, 56);
OFF(aster_reactor_call_t, request_flags, 60);
OFF(aster_reactor_call_t, peer_ptr, 64);
OFF(aster_reactor_call_t, peer_len, 72);
OFF(aster_reactor_call_t, header_buffer, 80);
OFF(aster_reactor_call_t, request_buffer, 88);
OFF(aster_reactor_call_t, peer_buffer, 96);
OFF(aster_reactor_call_t, connection_handle, 104);

/* ABI 3.1: AEv1 envelope structs. */
LAYOUT(aster_stream_header_t, 96);
OFF(aster_stream_header_t, struct_size, 0);
OFF(aster_stream_header_t, version, 4);
OFF(aster_stream_header_t, call_id, 8);
OFF(aster_stream_header_t, session_id, 12);
OFF(aster_stream_header_t, deadline, 16);
OFF(aster_stream_header_t, serialization_mode, 18);
OFF(aster_stream_header_t, reserved0, 19);
OFF(aster_stream_header_t, reserved1, 20);
OFF(aster_stream_header_t, service, 24);
OFF(aster_stream_header_t, method, 40);
OFF(aster_stream_header_t, metadata_keys, 56);
OFF(aster_stream_header_t, metadata_values, 72);
OFF(aster_stream_header_t, storage, 88);

LAYOUT(aster_call_header_t, 72);
OFF(aster_call_header_t, struct_size, 0);
OFF(aster_call_header_t, call_id, 4);
OFF(aster_call_header_t, deadline, 8);
OFF(aster_call_header_t, reserved0, 10);
OFF(aster_call_header_t, reserved1, 12);
OFF(aster_call_header_t, method, 16);
OFF(aster_call_header_t, metadata_keys, 32);
OFF(aster_call_header_t, metadata_values, 48);
OFF(aster_call_header_t, storage, 64);

LAYOUT(aster_rpc_status_t, 64);
OFF(aster_rpc_status_t, struct_size, 0);
OFF(aster_rpc_status_t, code, 4);
OFF(aster_rpc_status_t, message, 8);
OFF(aster_rpc_status_t, detail_keys, 24);
OFF(aster_rpc_status_t, detail_values, 40);
OFF(aster_rpc_status_t, storage, 56);

/* ABI 3.2: node configuration. */
LAYOUT(aster_config_t, 280);
OFF(aster_config_t, struct_size, 0);
OFF(aster_config_t, relay_mode, 4);
OFF(aster_config_t, relay_urls, 8);
OFF(aster_config_t, secret_key, 24);
OFF(aster_config_t, secret_key_file, 40);
OFF(aster_config_t, storage_path, 56);
OFF(aster_config_t, gc_interval_ms, 72);
OFF(aster_config_t, bind_addr, 80);
OFF(aster_config_t, alpns, 96);
OFF(aster_config_t, identity_file, 112);
OFF(aster_config_t, proxy_url, 128);
OFF(aster_config_t, hook_timeout_ms, 144);
OFF(aster_config_t, hook_failure_mode, 152);
OFF(aster_config_t, enable_hooks, 156);
OFF(aster_config_t, enable_monitoring, 160);
OFF(aster_config_t, local_discovery, 164);
OFF(aster_config_t, clear_ip_transports, 168);
OFF(aster_config_t, clear_relay_transports, 172);
OFF(aster_config_t, portmapper, 176);
OFF(aster_config_t, proxy_from_env, 180);
OFF(aster_config_t, transport_set, 184);
OFF(aster_config_t, transport_initial_mtu, 188);
OFF(aster_config_t, transport_send_fairness, 192);
OFF(aster_config_t, transport_enable_segmentation_offload, 196);
OFF(aster_config_t, transport_max_concurrent_bidi_streams, 200);
OFF(aster_config_t, transport_max_concurrent_uni_streams, 208);
OFF(aster_config_t, transport_stream_receive_window, 216);
OFF(aster_config_t, transport_receive_window, 224);
OFF(aster_config_t, transport_send_window, 232);
OFF(aster_config_t, transport_max_idle_timeout_ms, 240);
OFF(aster_config_t, transport_keep_alive_interval_ms, 248);
OFF(aster_config_t, transport_datagram_receive_buffer_size, 256);
OFF(aster_config_t, transport_datagram_send_buffer_size, 264);
OFF(aster_config_t, storage, 272);

/* ABI 3.3: connection paths (data.rs). */
_Static_assert(sizeof(aster_blob_format_t) == 4, "enum size");
_Static_assert(sizeof(aster_fetch_strategy_t) == 4, "enum size");
_Static_assert(sizeof(aster_local_access_t) == 4, "enum size");

LAYOUT(aster_path_info_t, 72);
OFF(aster_path_info_t, struct_size, 0);
OFF(aster_path_info_t, path_kind, 4);
OFF(aster_path_info_t, is_selected, 8);
OFF(aster_path_info_t, peer_port, 12);
OFF(aster_path_info_t, peer_host, 16);
OFF(aster_path_info_t, relay_url, 32);
OFF(aster_path_info_t, other, 48);
OFF(aster_path_info_t, rtt_micros, 64);

LAYOUT(aster_path_list_t, 32);
OFF(aster_path_list_t, struct_size, 0);
OFF(aster_path_list_t, items, 8);
OFF(aster_path_list_t, len, 16);
OFF(aster_path_list_t, storage, 24);

/* ABI 3.4: pure functions (pure.rs, grants.rs). */
_Static_assert(sizeof(aster_id_kind_t) == 4, "enum size");
_Static_assert(sizeof(aster_attestation_role_t) == 4, "enum size");
_Static_assert(sizeof(aster_grant_state_t) == 4, "enum size");
_Static_assert(sizeof(aster_grant_outcome_kind_t) == 4, "enum size");

LAYOUT(aster_attest_options_t, 32);
OFF(aster_attest_options_t, struct_size, 0);
OFF(aster_attest_options_t, reserved, 4);
OFF(aster_attest_options_t, epoch, 8);
OFF(aster_attest_options_t, not_before, 16);
OFF(aster_attest_options_t, not_after, 24);

LAYOUT(aster_verified_chain_t, 96);
OFF(aster_verified_chain_t, struct_size, 0);
OFF(aster_verified_chain_t, depth, 4);
OFF(aster_verified_chain_t, node, 8);
OFF(aster_verified_chain_t, anchor, 40);
OFF(aster_verified_chain_t, intermediates, 72);
OFF(aster_verified_chain_t, intermediates_count, 80);
OFF(aster_verified_chain_t, storage, 88);

LAYOUT(aster_grant_context_t, 104);
OFF(aster_grant_context_t, struct_size, 0);
OFF(aster_grant_context_t, reserved, 4);
OFF(aster_grant_context_t, app, 8);
OFF(aster_grant_context_t, granter, 24);
OFF(aster_grant_context_t, recipient, 40);
OFF(aster_grant_context_t, resource, 56);
OFF(aster_grant_context_t, path, 72);
OFF(aster_grant_context_t, role, 88);

LAYOUT(aster_grant_record_t, 72);
OFF(aster_grant_record_t, struct_size, 0);
OFF(aster_grant_record_t, state, 4);
OFF(aster_grant_record_t, context_version, 8);
OFF(aster_grant_record_t, reserved, 12);
OFF(aster_grant_record_t, role, 16);
OFF(aster_grant_record_t, envelope, 32);
OFF(aster_grant_record_t, reason, 48);
OFF(aster_grant_record_t, storage, 64);

LAYOUT(aster_grant_outcome_t, 72);
OFF(aster_grant_outcome_t, struct_size, 0);
OFF(aster_grant_outcome_t, kind, 4);
OFF(aster_grant_outcome_t, access, 8);
OFF(aster_grant_outcome_t, drops_capability, 12);
OFF(aster_grant_outcome_t, payload, 16);
OFF(aster_grant_outcome_t, reason, 32);
OFF(aster_grant_outcome_t, role, 48);
OFF(aster_grant_outcome_t, storage, 64);

/* ── Run-time checks ───────────────────────────────────────────────────── */

static int failures = 0;

#define CHECK(cond)                                                            \
    do {                                                                       \
        if (cond) {                                                            \
            printf("  ok   %s\n", #cond);                                      \
        } else {                                                               \
            printf("  FAIL %s  (%s:%d)\n", #cond, __FILE__, __LINE__);         \
            failures++;                                                        \
        }                                                                      \
    } while (0)

static void test_abi_version(void) {
    printf("[abi version]\n");
    uint32_t v = aster_abi_version();
    CHECK(v == ASTER_ABI_VERSION);
    CHECK((v >> 16) == ASTER_ABI_VERSION_MAJOR);
    CHECK(((v >> 8) & 0xff) == ASTER_ABI_VERSION_MINOR);
    CHECK((v & 0xff) == ASTER_ABI_VERSION_PATCH);
    CHECK(ASTER_ABI_VERSION_MAJOR == 3);
    CHECK(ASTER_ABI_VERSION_MINOR >= 4); /* 3.1 envelope + zstd, 3.2 node config + gate, 3.3 data plane, 3.4 pure functions */
    CHECK(aster_version() != NULL && strlen(aster_version()) > 0);
}

static void test_enum_values(void) {
    printf("[enum values]\n");
    CHECK(ASTER_STATUS_OK == 0);
    CHECK(ASTER_STATUS_QUEUE_FULL == 4);
    CHECK(ASTER_STATUS_STREAM_RESET == 11);
    CHECK(ASTER_STATUS_ABI_MISMATCH == 12);
    CHECK(ASTER_EVENT_NONE == 0);
    CHECK(ASTER_EVENT_CONNECTED == 10);
    CHECK(ASTER_EVENT_BLOB_OBSERVE_COMPLETE == 56);
    CHECK(ASTER_EVENT_ASTER_ACCEPTED == 65);
    CHECK(ASTER_EVENT_ERROR == 99);
    CHECK(ASTER_RELAY_MODE_DISABLED == 2);
    CHECK(ASTER_RELAY_MODE_STAGING == 3);
    CHECK(ASTER_HOOK_FAILURE_FAIL_CLOSED == 1);
    CHECK(ASTER_HOOK_DECISION_DENY == 1);
    CHECK(ASTER_CONNECTION_TYPE_UDP_RELAY == 3);
    CHECK(ASTER_PATH_KIND_RELAY == 2);
    CHECK(ASTER_DOC_QUERY_LATEST_PREFIX == 3);
    CHECK(ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT == 2);
    CHECK(ASTER_DOC_SHARE_WRITE == 1);
    CHECK(ASTER_REGISTRY_KEY_ACL == 5);
    CHECK(ASTER_REACTOR_EVENT_CALL == 0);
    CHECK(ASTER_REACTOR_EVENT_RESPONSE_CLOSED == 3);
    CHECK(ASTER_TRUST_MODE_PROTECTED == 0 && ASTER_TRUST_MODE_OPEN_DEV == 1);
    CHECK(ASTER_PORTMAPPER_DEFAULT == 0 && ASTER_PORTMAPPER_DISABLED == 2);
    CHECK(ASTER_TRANSPORT_INITIAL_MTU == (1 << 7));
    CHECK(ASTER_TRANSPORT_ALL == 0xfff);
    CHECK(strcmp(aster_status_name(ASTER_STATUS_ABI_MISMATCH), "ASTER_STATUS_ABI_MISMATCH") == 0);
    CHECK(strcmp(aster_status_name(-999), "ASTER_STATUS_UNKNOWN") == 0);
    /* 3.3 */
    CHECK(ASTER_EVENT_BLOB_FETCHED == 100 && ASTER_EVENT_STREAM_PRIORITY == 106);
    CHECK(ASTER_EVENT_DOC_OPENED == 103);
    CHECK(ASTER_PATH_KIND_OTHER == 3);
    CHECK(ASTER_BLOB_FORMAT_HASH_SEQ == 1 && ASTER_FETCH_STRATEGY_SPLIT_CHILDREN == 1);
    CHECK(ASTER_BLOB_STATUS_COMPLETE == 2);
    CHECK(ASTER_LOCAL_ACCESS_WRITE == 1 && ASTER_LOCAL_ACCESS_READ == 2);
    CHECK(ASTER_RETIRE_RETIRED == 0 && ASTER_RETIRE_BUSY == 1);
    CHECK(ASTER_FETCH_TO_STORE == 1 && ASTER_FETCH_REPORT_VERSION == 1);
    CHECK(ASTER_FETCH_REPORT_HEADER_SIZE == 16 && ASTER_FETCH_PROVIDER_RECORD_SIZE == 44);
    /* 3.4 */
    CHECK(ASTER_STATUS_VERIFICATION_FAILED == 13);
    CHECK(strcmp(aster_status_name(ASTER_STATUS_VERIFICATION_FAILED), "ASTER_STATUS_VERIFICATION_FAILED") == 0);
    CHECK(ASTER_ID_NODE_ID == 0 && ASTER_ID_HASH == 3 && ASTER_ID_AUTHOR_ID == 6);
    CHECK(ASTER_ATTESTATION_ROLE_NONE == 0 && ASTER_ATTESTATION_ROLE_INTERMEDIATE == 3);
    CHECK(ASTER_GRANT_STATE_ACTIVE == 0 && ASTER_GRANT_STATE_REVOKED == 1);
    CHECK(ASTER_GRANT_OUTCOME_ACTIVE == 0 && ASTER_GRANT_OUTCOME_ABSENT == 3);
    CHECK(ASTER_KEY_SIZE == 32 && ASTER_ID_HEX_LEN == 64);
}

static void test_data_plane(void) {
    printf("[data plane 3.3]\n");
    /* Pure: authority fingerprint (namespace 0x11.. , WRITE) is deterministic
     * and differs by access. */
    const char *ns = "1111111111111111111111111111111111111111111111111111111111111111";
    aster_bytes_t nsb = {(const uint8_t *)ns, 64};
    uint8_t w[32], r[32], w2[32];
    CHECK(aster_doc_authority_fingerprint(nsb, ASTER_LOCAL_ACCESS_WRITE, w) == ASTER_STATUS_OK);
    CHECK(aster_doc_authority_fingerprint(nsb, ASTER_LOCAL_ACCESS_WRITE, w2) == ASTER_STATUS_OK);
    CHECK(aster_doc_authority_fingerprint(nsb, ASTER_LOCAL_ACCESS_READ, r) == ASTER_STATUS_OK);
    CHECK(memcmp(w, w2, 32) == 0 && memcmp(w, r, 32) != 0);
    CHECK(aster_doc_authority_fingerprint(nsb, 0, w) == ASTER_STATUS_INVALID_ARGUMENT);
    aster_bytes_t shortb = {(const uint8_t *)ns, 10};
    CHECK(aster_doc_authority_fingerprint(shortb, ASTER_LOCAL_ACCESS_READ, w) == ASTER_STATUS_INVALID_ARGUMENT);

    /* Release of an empty / NULL path list is a no-op. */
    aster_path_list_t pl;
    memset(&pl, 0, sizeof pl);
    CHECK(aster_path_list_release(&pl) == ASTER_STATUS_OK);
    CHECK(aster_path_list_release(NULL) == ASTER_STATUS_OK);

    /* Validation happens before any node lookup. */
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_operation_t op = 0;
    aster_bytes_t prov = nsb;
    aster_bytes_list_t provs = {&prov, 1};
    CHECK(aster_blobs_download_hash_from(rt, 1, nsb, provs, ASTER_BLOB_FORMAT_RAW,
                                         ASTER_FETCH_STRATEGY_SPLIT_CHILDREN, 0, 0, &op) ==
          ASTER_STATUS_INVALID_ARGUMENT);
    aster_bytes_list_t none = {NULL, 0};
    CHECK(aster_blobs_download_hash_from(rt, 1, nsb, none, ASTER_BLOB_FORMAT_RAW,
                                         ASTER_FETCH_STRATEGY_WHOLE, 0, 0, &op) ==
          ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_blobs_read_range(rt, 424242, nsb, 0, 1, 0, &op) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_connection_paths(rt, 424242, &pl) == ASTER_STATUS_NOT_FOUND);
    CHECK(pl.len == 0 && pl.storage == NULL && pl.struct_size == sizeof(aster_path_list_t));
    CHECK(aster_send_stream_priority(rt, 424242, 0, &op) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

static void test_runtime(void) {
    printf("[runtime]\n");
    aster_runtime_t rt = 0;

    /* Wrong ABI major is refused, nothing is created. */
    int r = aster_runtime_new(ASTER_ABI_VERSION_MAJOR + 1, NULL, &rt);
    CHECK(r == ASTER_STATUS_ABI_MISMATCH);
    CHECK(rt == 0);
    char msg[256] = {0};
    size_t n = aster_last_error_message((uint8_t *)msg, sizeof msg - 1);
    CHECK(n > 0 && strstr(msg, "ABI") != NULL);

    /* struct_size is validated. */
    aster_runtime_config_t bad = {.struct_size = 4};
    r = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &bad, &rt);
    CHECK(r == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(rt == 0);

    aster_runtime_config_t cfg = {
        .struct_size = sizeof(aster_runtime_config_t),
        .worker_threads = 1,
        .event_queue_capacity = 64,
    };
    r = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &cfg, &rt);
    CHECK(r == ASTER_STATUS_OK);
    CHECK(rt != 0);

    CHECK(aster_buffer_release(rt, 999999) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_string_release(NULL, 0) == ASTER_STATUS_OK);

    aster_event_t events[4];
    memset(events, 0, sizeof events);
    CHECK(aster_poll_events(rt, events, 4, 0) == 0);

    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_NOT_FOUND);
}

static void test_envelope(void) {
    printf("[envelope]\n");
    /* Encode the envelope.json vector "RpcStatus: NOT_FOUND with details". */
    static const uint8_t want[] = {0xa3, 0x01, 0x0a, 0x26, 'n', 'o', 't', ' ', 'f', 'o', 'u',
                                   'n', 'd', 0x01, 0x0c, 0x22, 'r', 'e', 's', 'o', 'u', 'r',
                                   'c', 'e', 0x01, 0x0c, 0x1a, 't', 'a', 's', 'k', '/', '9'};
    aster_bytes_t key = {(const uint8_t *)"resource", 8};
    aster_bytes_t val = {(const uint8_t *)"task/9", 6};
    aster_rpc_status_t st = {
        .struct_size = sizeof(aster_rpc_status_t),
        .code = 5,
        .message = {(const uint8_t *)"not found", 9},
        .detail_keys = {&key, 1},
        .detail_values = {&val, 1},
    };
    aster_bytes_t out = {0};
    CHECK(aster_rpc_status_encode(&st, &out) == ASTER_STATUS_OK);
    CHECK(out.len == sizeof want && memcmp(out.ptr, want, sizeof want) == 0);
    CHECK(out.ptr[0] == ASTER_ENVELOPE_TAG_RPC_STATUS && out.ptr[1] == ASTER_ENVELOPE_VERSION);
    CHECK(aster_string_release(out.ptr, out.len) == ASTER_STATUS_OK);

    aster_rpc_status_t d;
    memset(&d, 0, sizeof d);
    CHECK(aster_rpc_status_decode(want, sizeof want, &d) == ASTER_STATUS_OK);
    CHECK(d.struct_size == sizeof(aster_rpc_status_t));
    CHECK(d.code == 5 && d.storage != NULL);
    CHECK(d.message.len == 9 && memcmp(d.message.ptr, "not found", 9) == 0);
    CHECK(d.detail_keys.len == 1 && d.detail_values.len == 1);
    CHECK(d.detail_values.items[0].len == 6 && memcmp(d.detail_values.items[0].ptr, "task/9", 6) == 0);
    CHECK(aster_rpc_status_release(&d) == ASTER_STATUS_OK);
    CHECK(d.storage == NULL && d.message.ptr == NULL);

    /* Wrong tag: StreamHeader decoder refuses RpcStatus bytes. */
    aster_stream_header_t h;
    CHECK(aster_stream_header_decode(want, sizeof want, &h) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(h.storage == NULL);

    /* zstd: at-threshold payload is not compressed; larger one round-trips. */
    static uint8_t buf[ASTER_COMPRESSION_THRESHOLD * 2];
    memset(buf, 'a', sizeof buf);
    aster_bytes_t c = {0};
    CHECK(aster_zstd_maybe_compress(buf, ASTER_COMPRESSION_THRESHOLD, ASTER_COMPRESSION_THRESHOLD, &c) == ASTER_STATUS_OK);
    CHECK(c.ptr == NULL && c.len == 0);
    CHECK(aster_zstd_maybe_compress(buf, sizeof buf, ASTER_COMPRESSION_THRESHOLD, &c) == ASTER_STATUS_OK);
    CHECK(c.ptr != NULL && c.len < sizeof buf);
    aster_bytes_t back = {0};
    CHECK(aster_zstd_decompress(c.ptr, c.len, &back) == ASTER_STATUS_OK);
    CHECK(back.len == sizeof buf && memcmp(back.ptr, buf, sizeof buf) == 0);
    CHECK(aster_string_release(back.ptr, back.len) == ASTER_STATUS_OK);
    CHECK(aster_string_release(c.ptr, c.len) == ASTER_STATUS_OK);
}

static void test_config(void) {
    printf("[config]\n");
    aster_config_t c;
    memset(&c, 0xab, sizeof c);
    CHECK(aster_config_init(&c) == ASTER_STATUS_OK);
    CHECK(c.struct_size == sizeof(aster_config_t));
    CHECK(c.storage == NULL);
    CHECK(c.relay_mode == ASTER_RELAY_MODE_DEFAULT);
    CHECK(c.hook_timeout_ms == 5000);
    CHECK(c.hook_failure_mode == ASTER_HOOK_FAILURE_FAIL_OPEN);
    CHECK(c.transport_set == 0 && c.bind_addr.ptr == NULL && c.alpns.len == 0);
    CHECK(aster_config_release(&c) == ASTER_STATUS_OK);

    aster_config_t f;
    aster_bytes_t missing = {(const uint8_t *)"/nonexistent/aster.toml", 23};
    CHECK(aster_config_from_toml(missing, &f) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(f.storage == NULL);
    CHECK(aster_config_init(NULL) == ASTER_STATUS_INVALID_ARGUMENT);
}

static int wait_event(aster_runtime_t rt, aster_operation_t op, aster_event_t *out) {
    for (int i = 0; i < 300; i++) {
        aster_event_t ev[8];
        size_t n = aster_poll_events(rt, ev, 8, 100);
        for (size_t k = 0; k < n; k++) {
            if (ev[k].operation == op) {
                *out = ev[k];
                return 1;
            }
            if (ev[k].buffer) aster_buffer_release(rt, ev[k].buffer);
        }
    }
    return 0;
}

static void test_gate_and_node(void) {
    printf("[gate + node start]\n");
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);

    aster_gate_t gate = 0;
    CHECK(aster_gate_new(rt, &gate) == ASTER_STATUS_OK && gate != 0);
    const char *peer = "abababababababababababababababababababababababababababababababab";
    aster_bytes_t p = {(const uint8_t *)peer, 64};
    uint32_t admitted = 9;
    CHECK(aster_gate_is_admitted(rt, gate, p, &admitted) == ASTER_STATUS_OK && admitted == 0);
    CHECK(aster_gate_admit(rt, gate, p) == ASTER_STATUS_OK);
    CHECK(aster_gate_is_admitted(rt, gate, p, &admitted) == ASTER_STATUS_OK && admitted == 1);
    CHECK(aster_gate_revoke(rt, gate, p) == ASTER_STATUS_OK);
    CHECK(aster_gate_is_admitted(rt, gate, p, &admitted) == ASTER_STATUS_OK && admitted == 0);
    aster_bytes_t reason = {(const uint8_t *)"blocked", 7};
    CHECK(aster_gate_set_reject(rt, gate, 451, reason) == ASTER_STATUS_OK);
    CHECK(aster_gate_set_trust_mode(rt, gate, 5) == ASTER_STATUS_INVALID_ARGUMENT);

    aster_config_t c;
    CHECK(aster_config_init(&c) == ASTER_STATUS_OK);
    c.relay_mode = ASTER_RELAY_MODE_DISABLED;
    c.bind_addr = (aster_bytes_t){(const uint8_t *)"127.0.0.1:0", 11};
    aster_bytes_t alpn = {(const uint8_t *)"aster/1", 7};
    c.alpns = (aster_bytes_list_t){&alpn, 1};
    aster_operation_t op = 0;
    CHECK(aster_node_start(rt, &c, gate, 42, &op) == ASTER_STATUS_OK);
    aster_event_t ev;
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    CHECK(ev.kind == ASTER_EVENT_NODE_CREATED && ev.user_data == 42 && ev.handle != 0);
    aster_node_t node = ev.handle;

    aster_bytes_t text = {0};
    CHECK(aster_node_metrics_prometheus(rt, node, &text) == ASTER_STATUS_OK);
    CHECK(text.ptr != NULL && strstr((const char *)text.ptr, "iroh_send_ipv4") != NULL);
    CHECK(aster_string_release(text.ptr, text.len) == ASTER_STATUS_OK);
    CHECK(aster_node_set_gate(rt, node, 0) == ASTER_STATUS_OK);
    CHECK(aster_node_set_gate(rt, node, gate) == ASTER_STATUS_OK);

    CHECK(aster_admission_respond(rt, 987654, ASTER_HOOK_DECISION_DENY, 403, reason) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_admission_respond(rt, 987654, 7, 0, reason) == ASTER_STATUS_INVALID_ARGUMENT);

    CHECK(aster_node_free(rt, node) == ASTER_STATUS_OK);
    CHECK(aster_gate_free(rt, gate) == ASTER_STATUS_OK);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

static aster_bytes_t bstr(const char *s) {
    aster_bytes_t b = {(const uint8_t *)s, strlen(s)};
    return b;
}

static void test_pure_functions(void) {
    printf("[pure functions 3.4]\n");
    /* Ids: secret -> public key, hex round-trip, validation. */
    uint8_t root_sk[32], node_sk[32], root_pk[32], node_pk[32], parsed[32];
    memset(root_sk, 1, 32);
    memset(node_sk, 2, 32);
    CHECK(aster_secret_key_public_key(root_sk, root_pk) == ASTER_STATUS_OK);
    CHECK(aster_secret_key_public_key(node_sk, node_pk) == ASTER_STATUS_OK);
    char hex[65];
    CHECK(aster_id_format(ASTER_ID_NODE_ID, node_pk, (uint8_t *)hex) == ASTER_STATUS_OK);
    CHECK(hex[64] == 0 && strlen(hex) == 64);
    CHECK(aster_id_parse(ASTER_ID_NODE_ID, bstr(hex), parsed) == ASTER_STATUS_OK);
    CHECK(memcmp(parsed, node_pk, 32) == 0);
    CHECK(aster_id_validate(ASTER_ID_NODE_ID, node_pk) == ASTER_STATUS_OK);
    CHECK(aster_id_parse(ASTER_ID_HASH, bstr("abc"), parsed) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_id_parse(42, bstr(hex), parsed) == ASTER_STATUS_INVALID_ARGUMENT);

    /* HPKE seal/open. */
    uint8_t hsk[32], hpk[32];
    CHECK(aster_hpke_generate_keypair(hsk, hpk) == ASTER_STATUS_OK);
    aster_bytes_t env = {0}, pt = {0};
    CHECK(aster_hpke_seal(hpk, bstr("aad"), bstr("secret"), &env) == ASTER_STATUS_OK);
    CHECK(aster_hpke_open(hsk, bstr("aad"), env, &pt) == ASTER_STATUS_OK);
    CHECK(pt.len == 6 && memcmp(pt.ptr, "secret", 6) == 0);
    CHECK(aster_string_release(pt.ptr, pt.len) == ASTER_STATUS_OK);
    CHECK(aster_hpke_open(hsk, bstr("other"), env, &pt) == ASTER_STATUS_VERIFICATION_FAILED);
    CHECK(pt.ptr == NULL && pt.len == 0);
    CHECK(aster_string_release(env.ptr, env.len) == ASTER_STATUS_OK);

    /* Attestation: mint, verify, roles. */
    aster_bytes_t chain = {0};
    CHECK(aster_attest_root_node(root_sk, node_sk, NULL, &chain) == ASTER_STATUS_OK);
    aster_verified_chain_t vc;
    memset(&vc, 0, sizeof vc);
    CHECK(aster_attestation_verify_chain(chain, root_pk, 1, node_pk, &vc) == ASTER_STATUS_OK);
    CHECK(vc.struct_size == sizeof(aster_verified_chain_t) && vc.depth == 1);
    CHECK(memcmp(vc.node, node_pk, 32) == 0 && memcmp(vc.anchor, root_pk, 32) == 0);
    uint32_t role = 99;
    CHECK(aster_verified_chain_role_of(&vc, root_pk, &role) == ASTER_STATUS_OK && role == ASTER_ATTESTATION_ROLE_ANCHOR);
    CHECK(aster_verified_chain_release(&vc) == ASTER_STATUS_OK);
    CHECK(aster_attestation_verify_chain(chain, node_pk, 1, node_pk, &vc) == ASTER_STATUS_VERIFICATION_FAILED);
    CHECK(aster_string_release(chain.ptr, chain.len) == ASTER_STATUS_OK);

    /* Grants: key, AAD, seal -> record -> reconcile, absent. */
    aster_bytes_t key = {0};
    CHECK(aster_grant_key(bstr("/app/v1/"), bstr(hex), &key) == ASTER_STATUS_OK);
    CHECK(key.len == 15 + 64 && memcmp(key.ptr, "/app/v1/grants/", 15) == 0);
    aster_grant_context_t ctx;
    memset(&ctx, 0, sizeof ctx);
    ctx.struct_size = sizeof ctx;
    ctx.app = bstr("app/v1");
    ctx.granter = bstr(hex);
    ctx.recipient = bstr(hex);
    ctx.resource = bstr("r");
    ctx.path = key;
    ctx.role = bstr("read");
    aster_bytes_t aad = {0};
    CHECK(aster_grant_context_aad(&ctx, &aad) == ASTER_STATUS_OK);
    CHECK(aad.len == 7 * 4 + 21 + 6 + 64 + 64 + 1 + key.len + 4);
    CHECK(aster_string_release(aad.ptr, aad.len) == ASTER_STATUS_OK);
    CHECK(aster_grant_seal(&ctx, bstr("cap"), &env) == ASTER_STATUS_OK);
    aster_grant_record_t rec;
    memset(&rec, 0, sizeof rec);
    rec.struct_size = sizeof rec;
    rec.state = ASTER_GRANT_STATE_ACTIVE;
    rec.context_version = 1;
    rec.role = ctx.role;
    rec.envelope = env;
    aster_bytes_t rec_bytes = {0};
    CHECK(aster_grant_record_encode(&rec, &rec_bytes) == ASTER_STATUS_OK);
    aster_grant_outcome_t out;
    memset(&out, 0, sizeof out);
    CHECK(aster_grant_reconcile(node_sk, &ctx, &rec_bytes, &out) == ASTER_STATUS_OK);
    CHECK(out.kind == ASTER_GRANT_OUTCOME_ACTIVE && out.drops_capability == 0);
    CHECK(out.payload.len == 3 && memcmp(out.payload.ptr, "cap", 3) == 0);
    CHECK(aster_grant_outcome_release(&out) == ASTER_STATUS_OK && out.storage == NULL);
    CHECK(aster_grant_reconcile(root_sk, &ctx, &rec_bytes, &out) == ASTER_STATUS_OK);
    CHECK(out.kind == ASTER_GRANT_OUTCOME_UNOPENABLE && out.drops_capability == 1 && out.reason.len > 0);
    CHECK(aster_grant_outcome_release(&out) == ASTER_STATUS_OK);
    CHECK(aster_grant_reconcile(node_sk, &ctx, NULL, &out) == ASTER_STATUS_OK);
    CHECK(out.kind == ASTER_GRANT_OUTCOME_ABSENT && out.storage == NULL);
    CHECK(aster_grant_check_role(bstr("read"), 1) == ASTER_STATUS_VERIFICATION_FAILED);
    CHECK(aster_grant_check_role(bstr("admin"), 1) == ASTER_STATUS_OK);
    ctx.struct_size = 8;
    CHECK(aster_grant_seal(&ctx, bstr("cap"), &aad) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_string_release(rec_bytes.ptr, rec_bytes.len) == ASTER_STATUS_OK);
    CHECK(aster_string_release(env.ptr, env.len) == ASTER_STATUS_OK);
    CHECK(aster_string_release(key.ptr, key.len) == ASTER_STATUS_OK);
}

int main(void) {
    test_abi_version();
    test_enum_values();
    test_runtime();
    test_envelope();
    test_config();
    test_gate_and_node();
    test_data_plane();
    test_pure_functions();
    if (failures) {
        printf("abi_contract_test: %d failure(s)\n", failures);
        return 1;
    }
    printf("abi_contract_test: all checks passed\n");
    return 0;
}
