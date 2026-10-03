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
    CHECK(ASTER_ABI_VERSION_MINOR >= 1); /* envelope + zstd */
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
    CHECK(strcmp(aster_status_name(ASTER_STATUS_ABI_MISMATCH), "ASTER_STATUS_ABI_MISMATCH") == 0);
    CHECK(strcmp(aster_status_name(-999), "ASTER_STATUS_UNKNOWN") == 0);
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

int main(void) {
    test_abi_version();
    test_enum_values();
    test_runtime();
    test_envelope();
    if (failures) {
        printf("abi_contract_test: %d failure(s)\n", failures);
        return 1;
    }
    printf("abi_contract_test: all checks passed\n");
    return 0;
}
