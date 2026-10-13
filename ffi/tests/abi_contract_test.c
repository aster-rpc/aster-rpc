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
#include <stdlib.h>
#include <string.h>
#include <time.h>

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

LAYOUT(aster_blob_custody_t, 8);
LAYOUT(aster_provisioning_result_t, 32);
OFF(aster_provisioning_result_t, data, 0);
OFF(aster_provisioning_result_t, error, 16);
_Static_assert(ASTER_EVENT_BLOB_CUSTODY_DONE == 118, "custody event value");
_Static_assert(ASTER_EVENT_DOC_CONTENTS_BATCH == 119, "batch content event value");
_Static_assert(ASTER_EVENT_TOPOLOGY_SNAPSHOT == 120, "topology snapshot event value");
_Static_assert(ASTER_TOPOLOGY_UNKNOWN == 0 && ASTER_TOPOLOGY_CONNECTED == 1 && ASTER_TOPOLOGY_SEPARATED == 2, "topology separation values");

LAYOUT(aster_bytes_t, 16);
OFF(aster_bytes_t, ptr, 0);
OFF(aster_bytes_t, len, 8);

LAYOUT(aster_bytes_list_t, 16);
OFF(aster_bytes_list_t, items, 0);
OFF(aster_bytes_list_t, len, 8);

LAYOUT(aster_raw_wt_config_t, 104);
OFF(aster_raw_wt_config_t, struct_size, 0);
OFF(aster_raw_wt_config_t, flags, 4);
OFF(aster_raw_wt_config_t, bind_addr, 8);
OFF(aster_raw_wt_config_t, certificate_pem, 24);
OFF(aster_raw_wt_config_t, private_key_pem, 40);
OFF(aster_raw_wt_config_t, path, 56);
OFF(aster_raw_wt_config_t, max_sessions, 72);
OFF(aster_raw_wt_config_t, max_streams, 76);
OFF(aster_raw_wt_config_t, max_datagrams, 80);
OFF(aster_raw_wt_config_t, max_datagram_bytes, 84);
OFF(aster_raw_wt_config_t, max_read_bytes, 88);
OFF(aster_raw_wt_config_t, max_pending_auth, 92);
OFF(aster_raw_wt_config_t, max_auth_payload_bytes, 96);
OFF(aster_raw_wt_config_t, reserved, 100);
_Static_assert(ASTER_EVENT_RAW_WT_SESSION == 121 && ASTER_EVENT_RAW_WT_ACKED == 126, "raw WT events");

LAYOUT(aster_runtime_config_t, 16);
OFF(aster_runtime_config_t, struct_size, 0);
OFF(aster_runtime_config_t, worker_threads, 4);
OFF(aster_runtime_config_t, event_queue_capacity, 8);
OFF(aster_runtime_config_t, max_inflight_operations, 12);

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
_Static_assert(sizeof(aster_grant_state_t) == 4, "enum size");
_Static_assert(sizeof(aster_grant_outcome_kind_t) == 4, "enum size");

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


/* ABI 3.5: dynamic dispatch */
LAYOUT(aster_dispatch_event_t, 104);
OFF(aster_dispatch_event_t, kind, 0);
OFF(aster_dispatch_event_t, pattern, 1);
OFF(aster_dispatch_event_t, reason, 2);
OFF(aster_dispatch_event_t, flags, 4);
OFF(aster_dispatch_event_t, call_id, 8);
OFF(aster_dispatch_event_t, service_idx, 16);
OFF(aster_dispatch_event_t, method_idx, 20);
OFF(aster_dispatch_event_t, session_key, 24);
OFF(aster_dispatch_event_t, connection_id, 32);
OFF(aster_dispatch_event_t, connection_handle, 40);
OFF(aster_dispatch_event_t, deadline_unix_ms, 48);
OFF(aster_dispatch_event_t, wire_call_id, 56);
OFF(aster_dispatch_event_t, session_id, 60);
OFF(aster_dispatch_event_t, request_ptr, 64);
OFF(aster_dispatch_event_t, request_len, 72);
OFF(aster_dispatch_event_t, peer_ptr, 80);
OFF(aster_dispatch_event_t, peer_len, 88);
OFF(aster_dispatch_event_t, buffer, 96);

LAYOUT(aster_call_options_t, 72);
OFF(aster_call_options_t, struct_size, 0);
OFF(aster_call_options_t, version, 4);
OFF(aster_call_options_t, service, 8);
OFF(aster_call_options_t, method, 24);
OFF(aster_call_options_t, metadata, 40);
OFF(aster_call_options_t, session_id, 56);
OFF(aster_call_options_t, deadline_ms, 60);
OFF(aster_call_options_t, serialization_mode, 64);
OFF(aster_call_options_t, wire_call_id, 68);

/* ABI 4.1: upcalls */
LAYOUT(aster_upcall_event_t, 40);
OFF(aster_upcall_event_t, invocation_id, 0);
OFF(aster_upcall_event_t, kind, 8);
OFF(aster_upcall_event_t, cancel_reason, 10);
OFF(aster_upcall_event_t, payload_len, 12);
OFF(aster_upcall_event_t, deadline_unix_ms, 16);
OFF(aster_upcall_event_t, payload_ptr, 24);
OFF(aster_upcall_event_t, buffer, 32);

LAYOUT(aster_topology_admitted_stats_t, 32);
OFF(aster_topology_admitted_stats_t, hits, 0);
OFF(aster_topology_admitted_stats_t, stale, 8);
OFF(aster_topology_admitted_stats_t, misses, 16);
OFF(aster_topology_admitted_stats_t, upcalls, 24);

/* ABI 4.2: expose */
LAYOUT(aster_http_foreign_options_t, 48);
OFF(aster_http_foreign_options_t, struct_size, 0);
OFF(aster_http_foreign_options_t, flags, 4);
OFF(aster_http_foreign_options_t, inline_request_max, 8);
OFF(aster_http_foreign_options_t, max_request_body, 16);
OFF(aster_http_foreign_options_t, max_response_body, 24);
OFF(aster_http_foreign_options_t, max_chunk, 32);
OFF(aster_http_foreign_options_t, response_buffer, 40);
OFF(aster_http_foreign_options_t, reserved, 44);
LAYOUT(aster_route_spec_t, 40);
OFF(aster_route_spec_t, host, 0);
OFF(aster_route_spec_t, service_id, 16);
OFF(aster_route_spec_t, port, 32);
OFF(aster_route_spec_t, protocol, 36);
_Static_assert(sizeof(aster_http_body_status_t) == 4, "enum size");
_Static_assert(sizeof(aster_route_protocol_t) == 4, "enum size");
_Static_assert(sizeof(aster_expose_t) == 8 && sizeof(aster_http_bodies_t) == 8 &&
               sizeof(aster_route_registration_t) == 8, "handle size");

/* ABI 4.3: streaming consumer relay */
LAYOUT(aster_http_relay_options_t, 24);
OFF(aster_http_relay_options_t, struct_size, 0);
OFF(aster_http_relay_options_t, flags, 4);
OFF(aster_http_relay_options_t, max_chunk, 8);
OFF(aster_http_relay_options_t, upload_capacity, 16);
OFF(aster_http_relay_options_t, reserved, 20);

/* ABI 4.4: coordination client */
LAYOUT(aster_coordination_client_options_t, 48);
OFF(aster_coordination_client_options_t, struct_size, 0);
OFF(aster_coordination_client_options_t, flags, 4);
OFF(aster_coordination_client_options_t, group_id, 8);
OFF(aster_coordination_client_options_t, seed_tickets, 16);
OFF(aster_coordination_client_options_t, journal_dir, 32);
_Static_assert(sizeof(aster_coordination_error_t) == 4, "enum size");
_Static_assert(sizeof(aster_coordination_mutation_op_t) == 4, "enum size");
_Static_assert(sizeof(aster_coordination_read_op_t) == 4, "enum size");
_Static_assert(sizeof(aster_coordination_client_t) == 8 && sizeof(aster_coordination_resolved_t) == 8 &&
               sizeof(aster_coordination_lease_t) == 8 &&
               sizeof(aster_coordination_placement_manager_t) == 8, "handle size");

/* ABI 4.6: observability and health */
LAYOUT(aster_observability_config_t, 128);
OFF(aster_observability_config_t, struct_size, 0);
OFF(aster_observability_config_t, flags, 4);
OFF(aster_observability_config_t, service_name, 8);
OFF(aster_observability_config_t, otlp_endpoint, 24);
OFF(aster_observability_config_t, otlp_headers, 40);
OFF(aster_observability_config_t, resource_attrs, 56);
OFF(aster_observability_config_t, log_filter, 72);
OFF(aster_observability_config_t, extra_toml, 88);
OFF(aster_observability_config_t, trace_sample_ratio, 104);
OFF(aster_observability_config_t, protocol, 112);
OFF(aster_observability_config_t, log_queue_capacity, 116);
OFF(aster_observability_config_t, binding, 120);
LAYOUT(aster_log_record_t, 96);
OFF(aster_log_record_t, level, 0);
OFF(aster_log_record_t, flags, 1);
OFF(aster_log_record_t, timestamp_unix_ns, 8);
OFF(aster_log_record_t, trace_id, 16);
OFF(aster_log_record_t, span_id, 32);
OFF(aster_log_record_t, call_id, 40);
OFF(aster_log_record_t, target_ptr, 48);
OFF(aster_log_record_t, target_len, 56);
OFF(aster_log_record_t, message_ptr, 64);
OFF(aster_log_record_t, message_len, 72);
OFF(aster_log_record_t, fields_ptr, 80);
OFF(aster_log_record_t, fields_len, 88);
LAYOUT(aster_log_emit_t, 72);
OFF(aster_log_emit_t, level, 4);
OFF(aster_log_emit_t, timestamp_unix_ns, 8);
OFF(aster_log_emit_t, logger, 16);
OFF(aster_log_emit_t, message, 32);
OFF(aster_log_emit_t, fields, 48);
OFF(aster_log_emit_t, context, 64);
LAYOUT(aster_instrument_desc_t, 96);
OFF(aster_instrument_desc_t, kind, 4);
OFF(aster_instrument_desc_t, name, 8);
OFF(aster_instrument_desc_t, description, 24);
OFF(aster_instrument_desc_t, unit, 40);
OFF(aster_instrument_desc_t, auto_attrs, 56);
OFF(aster_instrument_desc_t, cardinality_limit, 72);
OFF(aster_instrument_desc_t, boundaries, 80);
OFF(aster_instrument_desc_t, boundaries_len, 88);
LAYOUT(aster_span_context_t, 32);
OFF(aster_span_context_t, trace_id, 0);
OFF(aster_span_context_t, span_id, 16);
OFF(aster_span_context_t, trace_flags, 24);
OFF(aster_span_context_t, is_remote, 25);
LAYOUT(aster_span_record_t, 152);
OFF(aster_span_record_t, context, 0);
OFF(aster_span_record_t, parent, 32);
OFF(aster_span_record_t, start_unix_nanos, 64);
OFF(aster_span_record_t, end_unix_nanos, 72);
OFF(aster_span_record_t, kind, 80);
OFF(aster_span_record_t, status_code, 84);
OFF(aster_span_record_t, name, 88);
OFF(aster_span_record_t, status_message, 104);
OFF(aster_span_record_t, kv, 120);
OFF(aster_span_record_t, kv_len, 128);
OFF(aster_span_record_t, events, 136);
OFF(aster_span_record_t, events_len, 144);
LAYOUT(aster_generated_cert_t, 64);
OFF(aster_generated_cert_t, cert_pem, 0);
OFF(aster_generated_cert_t, key_pem, 16);
OFF(aster_generated_cert_t, sha256, 32);

LAYOUT(aster_http_server_config_t, 64);
OFF(aster_http_server_config_t, flags, 4);
OFF(aster_http_server_config_t, bind_addr, 8);
OFF(aster_http_server_config_t, cert_pem, 24);
OFF(aster_http_server_config_t, key_pem, 40);
OFF(aster_http_server_config_t, max_pending_auth, 56);
OFF(aster_http_server_config_t, max_auth_payload_bytes, 60);
_Static_assert(ASTER_HTTP_SERVER_WEBTRANSPORT == 1, "WT config flag");
_Static_assert(ASTER_UPCALL_KIND_RPC_AUTHENTICATE == 80, "RPC authentication kind");
LAYOUT(aster_health_config_t, 24);
OFF(aster_health_config_t, port, 4);
OFF(aster_health_config_t, host, 8);
_Static_assert(sizeof(aster_binding_kind_t) == 4 && sizeof(aster_otel_protocol_t) == 4 &&
               sizeof(aster_log_level_t) == 4 && sizeof(aster_instrument_kind_t) == 4 &&
               sizeof(aster_span_kind_t) == 4, "enum size");
_Static_assert(sizeof(aster_instrument_t) == 8 && sizeof(aster_attrset_t) == 8 &&
               sizeof(aster_span_t) == 8 && sizeof(aster_health_t) == 8, "handle size");

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
    CHECK(ASTER_ABI_VERSION_MINOR == 1); /* 5.1: Enrollment (additive) */
    CHECK(ASTER_EVENT_ENROLLMENT_RESULT == 127);
    CHECK(ASTER_ENROLLMENT_ERROR_DENIED == 2 && ASTER_ENROLLMENT_ERROR_UNAVAILABLE == 4);
    CHECK(aster_enrollment_free(0, 0) != ASTER_STATUS_OK);
    CHECK(ASTER_ENROLLMENT_SIDE_SERVER == 0 && ASTER_ENROLLMENT_SIDE_CLIENT == 1);
    {
        aster_operation_t op = 0;
        aster_bytes_t empty = {NULL, 0};
        CHECK(aster_node_start_enrolled(0, NULL, empty, 0, 0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
        CHECK(aster_node_start_enrolled(0, NULL, empty, 7, 0, &op) == ASTER_STATUS_INVALID_ARGUMENT);
        CHECK(aster_config_enrollment_from_env(NULL) == ASTER_STATUS_INVALID_ARGUMENT);
        CHECK(aster_lease_serializer_grantor_new(0, 0, 0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
        CHECK(aster_server_add_lease_grantor(0, 0, empty, empty) == ASTER_STATUS_NOT_FOUND);
    }
    CHECK(aster_doc_event_receiver_free(0, 0) == ASTER_STATUS_NOT_FOUND);
    CHECK(sizeof(aster_audit_outbox_error_t) == 4);
    CHECK(sizeof(aster_audit_outbox_t) == 8);
    CHECK(ASTER_EVENT_AUDIT_OUTBOX_OPENED == 114);
    CHECK(ASTER_EVENT_AUDIT_OUTBOX_EXPORTED == 117);
    CHECK(ASTER_ABI_VERSION_MAJOR == 5); /* 5.0: Fory ownership-attestation functions removed */
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
    CHECK(ASTER_GRANT_STATE_ACTIVE == 0 && ASTER_GRANT_STATE_REVOKED == 1);
    CHECK(ASTER_GRANT_OUTCOME_ACTIVE == 0 && ASTER_GRANT_OUTCOME_ABSENT == 3);
    CHECK(ASTER_KEY_SIZE == 32 && ASTER_ID_HEX_LEN == 64);
    /* 3.5 */
    CHECK(ASTER_DISPATCH_EVENT_CALL == 0 && ASTER_DISPATCH_EVENT_CANCELLED == 1);
    CHECK(ASTER_DISPATCH_EVENT_RESPONSE_WRITABLE == 2 && ASTER_DISPATCH_EVENT_RESPONSE_CLOSED == 3);
    CHECK(ASTER_DISPATCH_EVENT_SESSION_CLOSED == 4 && ASTER_DISPATCH_EVENT_CONNECTION_CLOSED == 5);
    CHECK(ASTER_DISPATCH_EVENT_NON_RPC == 6);
    CHECK(ASTER_CANCEL_REASON_PEER == 0 && ASTER_CANCEL_REASON_DEADLINE == 1 && ASTER_CANCEL_REASON_INVALID_REQUEST == 2);
    CHECK(ASTER_METHOD_PATTERN_UNARY == 0 && ASTER_METHOD_PATTERN_BIDI_STREAM == 3);
    CHECK(ASTER_SERVER_OPTION_BUILTIN_NODE_INFO == 1 && ASTER_SERVER_OPTION_HANDLER_TIMEOUT_MS == 4);
    CHECK(ASTER_SERVER_OPTION_MAX_FOREIGN_INFLIGHT == 5);
    CHECK(ASTER_DISPATCH_FLAG_FIRST_IS_END == 1 && ASTER_DISPATCH_FLAG_NEW_SESSION == 2);
    CHECK(ASTER_DISPATCH_FLAG_IROH_AUTHENTICATED == 4);
    CHECK(ASTER_DISPATCH_FLAG_HAS_METADATA == 0x100 && ASTER_DISPATCH_FLAG_HAS_ATTRIBUTES == 0x200);
    CHECK(ASTER_TIMEOUT_INFINITE == 0xffffffffu);
    /* 4.1 */
    CHECK(ASTER_EVENT_LEASE_ACQUIRED == 107 && ASTER_EVENT_LEASE_STATUS == 108);
    CHECK(ASTER_UPCALL_KIND_CANCEL == 0 && ASTER_UPCALL_KIND_LEASE_TRANSITION == 1);
    CHECK(ASTER_UPCALL_KIND_FENCED_MUTATE == 4 && ASTER_UPCALL_KIND_TOPOLOGY_ADMITTED == 16);
    CHECK(ASTER_UPCALL_KIND_PLACEMENT_PROVISION == 33 && ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE == 48);
    CHECK(ASTER_UPCALL_KIND_EXPOSE_HTTP == 49 && ASTER_UPCALL_KIND_EXPOSE_ROUTE_POLICY == 50);
    CHECK(ASTER_UPCALL_KIND_ADMISSION_HANDSHAKE == 65);
    CHECK(ASTER_UPCALL_STATUS_OK == 0 && ASTER_UPCALL_STATUS_ERROR == 1 && ASTER_UPCALL_STATUS_UNIMPLEMENTED == 2);
    CHECK(ASTER_UPCALL_CANCEL_TIMEOUT == 1 && ASTER_UPCALL_CANCEL_SHUTDOWN == 3);
    CHECK(ASTER_LEASE_STATUS_HELD == 0 && ASTER_LEASE_STATUS_LOST == 3);
    CHECK(ASTER_UPCALL_DEFAULT_CAPACITY == 1024);
    /* 4.4 */
    CHECK(ASTER_EVENT_COORDINATION_CLIENT_OPENED == 110 && ASTER_EVENT_COORDINATION_RESOLVED == 111);
    CHECK(ASTER_EVENT_COORDINATION_RESULT == 112 && ASTER_EVENT_COORDINATION_LEASE_ACQUIRED == 113);
    CHECK(ASTER_COORDINATION_ERROR_AWAITING_ACK == 1 && ASTER_COORDINATION_ERROR_JOURNAL_LOCKED == 3);
    CHECK(ASTER_COORDINATION_ERROR_RPC == 5 && ASTER_COORDINATION_ERROR_UNAVAILABLE == 6);
    CHECK(ASTER_COORDINATION_ERROR_CLOSED == 7 && ASTER_COORDINATION_ERROR_OTHER == 10);
    CHECK(ASTER_COORDINATION_MUTATE_ACQUIRE == 1 && ASTER_COORDINATION_MUTATE_CANCEL_MEMBERSHIP == 10);
    CHECK(ASTER_COORDINATION_READ_READ_LINEARIZABLE == 1 && ASTER_COORDINATION_READ_RECOVERY_EXPORT == 8);
    CHECK(ASTER_COORDINATION_TAG_MUTATION_REQUEST == 0xD0 && ASTER_COORDINATION_TAG_PLACEMENT_REPORT == 0xDE);
    CHECK(ASTER_COORDINATION_CLIENT_READ_ONLY == 1);
    /* 4.6 */
    CHECK(ASTER_STATUS_ALREADY_EXISTS == 17);
    CHECK(strcmp(aster_status_name(ASTER_STATUS_ALREADY_EXISTS), "ASTER_STATUS_ALREADY_EXISTS") == 0);
    CHECK(ASTER_STATUS_OPERATION_LIMIT == 18);
    CHECK(strcmp(aster_status_name(ASTER_STATUS_OPERATION_LIMIT), "ASTER_STATUS_OPERATION_LIMIT") == 0);
    CHECK(ASTER_BINDING_UNKNOWN == 0 && ASTER_BINDING_GO == 6);
    CHECK(ASTER_OTEL_PROTOCOL_DEFAULT == 0 && ASTER_OTEL_PROTOCOL_GRPC == 2);
    CHECK(ASTER_OBSERVABILITY_FROM_ENV == 1 && ASTER_OBSERVABILITY_TRACES == 2 && ASTER_OBSERVABILITY_METRICS == 4);
    CHECK(ASTER_OBSERVABILITY_OTLP_LOGS == 8 && ASTER_OBSERVABILITY_LOG_BRIDGE == 16);
    CHECK(ASTER_LOG_ERROR == 1 && ASTER_LOG_TRACE == 5);
    CHECK(ASTER_LOG_FLAG_HAS_TRACE == 1 && ASTER_LOG_FLAG_IN_CALL == 2);
    CHECK(ASTER_LOG_FLAG_SYNTHETIC == 4 && ASTER_LOG_FLAG_AUDIT == 8);
    CHECK(ASTER_INSTRUMENT_COUNTER_U64 == 1 && ASTER_INSTRUMENT_GAUGE_F64 == 4);
    CHECK(ASTER_SPAN_INTERNAL == 0 && ASTER_SPAN_CONSUMER == 4);
    CHECK(ASTER_DISPATCH_FLAG_HAS_TRACE == 0x400);
    CHECK(ASTER_DISPATCH_FLAG_HAS_INBOUND_TRACE == 0x800);
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

static void test_dispatch_server(void) {
    printf("[dispatch server (3.5)]\n");
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_config_t c;
    CHECK(aster_config_init(&c) == ASTER_STATUS_OK);
    c.relay_mode = ASTER_RELAY_MODE_DISABLED;
    c.bind_addr = (aster_bytes_t){(const uint8_t *)"127.0.0.1:0", 11};
    aster_bytes_t alpn = {(const uint8_t *)"aster/1", 7};
    c.alpns = (aster_bytes_list_t){&alpn, 1};
    aster_operation_t op = 0;
    CHECK(aster_node_start(rt, &c, 0, 7, &op) == ASTER_STATUS_OK);
    aster_event_t ev;
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    CHECK(ev.kind == ASTER_EVENT_NODE_CREATED && ev.handle != 0);
    aster_node_t node = ev.handle;

    static const char json[] =
        "{\"name\":\"c.Echo\",\"version\":1,\"methods\":[{\"name\":\"echo\","
        "\"pattern\":\"unary\",\"request\":{\"hash\":"
        "\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"},"
        "\"response\":{\"hash\":"
        "\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"}}]}";
    aster_bytes_t cid = {0};
    CHECK(aster_dynamic_service_contract_id((const uint8_t *)json, sizeof json - 1, &cid) == ASTER_STATUS_OK);
    CHECK(cid.len == 64);
    CHECK(aster_string_release(cid.ptr, cid.len) == ASTER_STATUS_OK);

    aster_server_builder_t b = 0;
    CHECK(aster_server_builder_new(rt, node, &b) == ASTER_STATUS_OK && b != 0);
    uint32_t idx = 99;
    CHECK(aster_server_add_service_json(b, (const uint8_t *)json, sizeof json - 1, &idx) == ASTER_STATUS_OK && idx == 0);
    CHECK(aster_server_add_service_json(b, (const uint8_t *)"{}", 2, &idx) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_server_set_option(b, ASTER_SERVER_OPTION_MAX_SESSIONS_PER_CONNECTION, 16) == ASTER_STATUS_OK);
    CHECK(aster_server_set_option(b, 77, 1) == ASTER_STATUS_INVALID_ARGUMENT);
    aster_server_t srv = 0;
    CHECK(aster_server_start(rt, b, 32, &srv) == ASTER_STATUS_OK && srv != 0);
    CHECK(aster_server_start(rt, b, 32, &srv) == ASTER_STATUS_NOT_FOUND);

    aster_dispatch_event_t events[4];
    CHECK(aster_server_poll(rt, srv, events, 4, 0) == 0);
    uint64_t required = 0;
    CHECK(aster_dispatch_respond(rt, srv, 1, NULL, 0, NULL, 0, &required) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_dispatch_send(rt, srv, 1, (const uint8_t *)"x", 1, &required) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_dispatch_is_cancelled(rt, srv, 1) == 1);
    CHECK(aster_dispatch_buffer_release(rt, srv, 0) == ASTER_STATUS_OK);
    CHECK(aster_server_live_calls(srv) == 0);
    CHECK(aster_server_destroy(rt, srv) == ASTER_STATUS_OK);
    CHECK(aster_server_destroy(rt, srv) == ASTER_STATUS_NOT_FOUND);

    aster_call_options_t o;
    memset(&o, 0, sizeof o);
    o.struct_size = sizeof o;
    aster_call_t call = 0;
    CHECK(aster_rpc_open(rt, 424242, ASTER_METHOD_PATTERN_BIDI_STREAM, &o, &call) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_call_cancel(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_call_release(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_call_discard(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_call_buffer_release(rt, 0) == ASTER_STATUS_OK);
    /* 4.0: aster_accept takes a node handle. */
    aster_operation_t accept_op = 0;
    CHECK(aster_accept(rt, 424242, 0, &accept_op) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_accept(rt, 424242, 0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

static void test_upcalls(void) {
    printf("[upcalls 4.1]\n");
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_upcalls_t up = 0;
    CHECK(aster_upcalls_new(rt, 0, &up) == ASTER_STATUS_OK && up != 0);
    CHECK(aster_upcall_set_policy(rt, up, ASTER_UPCALL_KIND_LEASE_TRANSITION, 5000,
                                  ASTER_HOOK_FAILURE_FAIL_CLOSED) == ASTER_STATUS_OK);
    CHECK(aster_upcall_set_policy(rt, up, ASTER_UPCALL_KIND_CANCEL, 5000, 0) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_upcall_set_default_policy(rt, up, 0, 0) == ASTER_STATUS_INVALID_ARGUMENT);

    aster_upcall_event_t ev[4];
    uint32_t n = 7;
    CHECK(aster_upcall_poll(rt, up, ev, 4, 0, &n) == ASTER_STATUS_OK && n == 0);
    CHECK(aster_upcall_poll(rt, up, ev, 4, 0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_upcall_poll(rt, up + 424242, ev, 4, 0, &n) == ASTER_STATUS_NOT_FOUND && n == 0);

    /* A lease whose serializer is "this program": answer the ACQUIRE by hand. */
    aster_lease_serializer_t ser = 0;
    CHECK(aster_lease_serializer_foreign_new(rt, up, &ser) == ASTER_STATUS_OK && ser != 0);
    aster_operation_t op = 0;
    CHECK(aster_lease_acquire(rt, ser, bstr("r"), bstr("a"), 0, 0, 0, 9, &op) == ASTER_STATUS_OK);
    CHECK(aster_upcall_poll(rt, up, ev, 4, 5000, &n) == ASTER_STATUS_OK);
    CHECK(n == 1);
    CHECK(ev[0].kind == ASTER_UPCALL_KIND_LEASE_TRANSITION && ev[0].invocation_id != 0);
    CHECK(ev[0].payload_len > 2 && ev[0].payload_ptr[0] == 0xB1 && ev[0].payload_ptr[1] == 0x01);
    CHECK(ev[0].deadline_unix_ms > 0 && ev[0].buffer != 0);
    CHECK(aster_upcall_buffer_release(rt, up, ev[0].buffer) == ASTER_STATUS_OK);
    CHECK(aster_upcall_buffer_release(rt, up, ev[0].buffer) == ASTER_STATUS_NOT_FOUND);
    /* UCv1 TransitionOutcome: granted, Fence { "r", epoch 1, "a" }. */
    const uint8_t granted[] = {0xB2, 0x01, 0x01, 0x06, 'r', 0x01, 0x06, 'a'};
    CHECK(aster_upcall_respond(rt, up, ev[0].invocation_id, ASTER_UPCALL_STATUS_OK, granted,
                               sizeof granted) == ASTER_STATUS_OK);
    CHECK(aster_upcall_respond(rt, up, ev[0].invocation_id, ASTER_UPCALL_STATUS_OK, granted,
                               sizeof granted) == ASTER_STATUS_NOT_FOUND);
    aster_event_t done;
    memset(&done, 0, sizeof done);
    CHECK(wait_event(rt, op, &done));
    CHECK(done.kind == ASTER_EVENT_LEASE_ACQUIRED && done.status == ASTER_STATUS_OK);
    CHECK(done.user_data == 9 && done.handle != 0);
    CHECK(done.data_len == sizeof granted && memcmp(done.data_ptr, granted, sizeof granted) == 0);
    if (done.buffer) aster_buffer_release(rt, done.buffer);
    aster_lease_t lease = done.handle;
    uint64_t epoch = 0;
    uint32_t status = 9;
    CHECK(aster_lease_fence(rt, lease, &epoch) == ASTER_STATUS_OK && epoch == 1);
    CHECK(aster_lease_status(rt, lease, &status) == ASTER_STATUS_OK && status == ASTER_LEASE_STATUS_HELD);
    CHECK(aster_lease_free(rt, lease) == ASTER_STATUS_OK);
    CHECK(aster_lease_free(rt, lease) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_lease_serializer_free(rt, ser) == ASTER_STATUS_OK);

    aster_topology_admitted_t adm = 0;
    CHECK(aster_topology_admitted_new(rt, up, 0, 0, 0, &adm) == ASTER_STATUS_OK && adm != 0);
    aster_topology_admitted_stats_t st;
    memset(&st, 0xff, sizeof st);
    CHECK(aster_topology_admitted_stats(rt, adm, &st) == ASTER_STATUS_OK && st.hits == 0 && st.upcalls == 0);
    CHECK(aster_topology_admitted_free(rt, adm) == ASTER_STATUS_OK);

    CHECK(aster_upcalls_close(rt, up) == ASTER_STATUS_OK);
    CHECK(aster_upcalls_free(rt, up) == ASTER_STATUS_OK);
    CHECK(aster_upcalls_free(rt, up) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

static void test_expose(void) {
    printf("[expose 4.2]\n");
    /* BodyError::code() is the stable ABI value of each body status. */
    CHECK(ASTER_HTTP_BODY_OK == 0 && ASTER_HTTP_BODY_UNKNOWN_STREAM == 1 &&
          ASTER_HTTP_BODY_CANCELLED == 2 && ASTER_HTTP_BODY_TOO_LARGE == 3 &&
          ASTER_HTTP_BODY_CHUNK_TOO_LARGE == 4 && ASTER_HTTP_BODY_FINISHED == 5 &&
          ASTER_HTTP_BODY_TIMEOUT == 6 && ASTER_HTTP_BODY_READ == 7 &&
          ASTER_HTTP_BODY_INVALID_TRAILERS == 8 && ASTER_HTTP_BODY_BLOCKING_ON_RUNTIME == 9);
    CHECK(ASTER_HTTP_BODY_INVALID_ARGUMENT == -1 && ASTER_HTTP_BODY_UNKNOWN_HANDLE == -2);
    CHECK(ASTER_ROUTE_PROTOCOL_HTTP == 0 && ASTER_ROUTE_PROTOCOL_TCP == 1);
    CHECK(ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE == 48 && ASTER_UPCALL_KIND_EXPOSE_HTTP == 49 &&
          ASTER_UPCALL_KIND_EXPOSE_ROUTE_POLICY == 50);

    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_http_foreign_options_t o;
    memset(&o, 0xff, sizeof o);
    CHECK(aster_http_foreign_options_init(&o) == ASTER_STATUS_OK);
    CHECK(o.struct_size == sizeof o && o.flags == 0 && o.max_chunk == 0 && o.response_buffer == 0);

    aster_http_bodies_t bodies = 0;
    CHECK(aster_http_bodies_new(rt, &bodies) == ASTER_STATUS_OK && bodies != 0);
    uint64_t open = 9;
    CHECK(aster_http_bodies_open_count(rt, bodies, &open) == ASTER_STATUS_OK && open == 0);
    uint8_t buf[16];
    size_t got = 7;
    uint32_t flag = 7;
    /* No exchange: UnknownStream; bad arguments are negative, not aster_status_t. */
    CHECK(aster_http_body_read(rt, bodies, 99, buf, sizeof buf, 0, &got, &flag) ==
          ASTER_HTTP_BODY_UNKNOWN_STREAM);
    CHECK(got == 0 && flag == 0);
    CHECK(aster_http_body_read(rt, bodies, 99, buf, 0, 0, &got, &flag) == ASTER_HTTP_BODY_INVALID_ARGUMENT);
    CHECK(aster_http_body_write(rt, bodies, 99, (const uint8_t *)"x", 1, 10) == ASTER_HTTP_BODY_UNKNOWN_STREAM);
    CHECK(aster_http_body_write(rt, bodies, 99, NULL, 1, 10) == ASTER_HTTP_BODY_INVALID_ARGUMENT);
    CHECK(aster_http_body_try_write(rt, bodies, 99, (const uint8_t *)"x", 1, &flag) == ASTER_HTTP_BODY_UNKNOWN_STREAM);
    CHECK(aster_http_body_finish(rt, bodies, 99, NULL, 0) == ASTER_HTTP_BODY_UNKNOWN_STREAM);
    CHECK(aster_http_body_abort(rt, bodies, 99, (const uint8_t *)"no", 2) == ASTER_HTTP_BODY_UNKNOWN_STREAM);
    CHECK(aster_http_body_finish(rt, 424242, 1, NULL, 0) == ASTER_HTTP_BODY_UNKNOWN_HANDLE);

    aster_expose_t ex = 5;
    CHECK(aster_expose_new(rt, 424242, &ex) == ASTER_STATUS_NOT_FOUND && ex == 0);
    CHECK(aster_expose_free(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_expose_unexpose(rt, 424242, bstr("svc")) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_route_registration_free(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_http_bodies_free(rt, bodies) == ASTER_STATUS_OK);
    CHECK(aster_http_bodies_free(rt, bodies) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

/* A loopback node for the relay round trip. Returns 0 on failure. */
static aster_node_t relay_node(aster_runtime_t rt, aster_bytes_t *alpn) {
    aster_config_t c;
    if (aster_config_init(&c) != ASTER_STATUS_OK) return 0;
    c.relay_mode = ASTER_RELAY_MODE_DISABLED;
    c.bind_addr = bstr("127.0.0.1:0");
    c.alpns = (aster_bytes_list_t){alpn, 1};
    aster_operation_t op = 0;
    if (aster_node_start(rt, &c, 0, 0, &op) != ASTER_STATUS_OK) return 0;
    aster_event_t ev;
    memset(&ev, 0, sizeof ev);
    if (!wait_event(rt, op, &ev) || ev.kind != ASTER_EVENT_NODE_CREATED) return 0;
    return ev.handle;
}

/* Answer the server's upcalls (kind 48: allow; kind 49: an inline 201
 * "pong") until `response` has its head. A single thread does both sides:
 * the relay calls below never block it except with a timeout. */
static int serve_until_head(aster_runtime_t rt, aster_upcalls_t up, aster_http_response_t response) {
    /* UCv1 CF Decision(allow) and C5 { 201, no headers, Inline "pong" }. */
    static const uint8_t allow[] = {0xCF, 0x01, 0x01};
    static const uint8_t pong[] = {0xC5, 0x01, 0xC9, 0x01, 0x00, 0x0C, 0x00, 0x04, 'p', 'o', 'n', 'g'};
    for (int i = 0; i < 200; i++) {
        aster_upcall_event_t ev[4];
        uint32_t n = 0;
        if (aster_upcall_poll(rt, up, ev, 4, 50, &n) != ASTER_STATUS_OK) n = 0;
        for (uint32_t k = 0; k < n; k++) {
            if (ev[k].kind == ASTER_UPCALL_KIND_EXPOSE_AUTHORIZE) {
                aster_upcall_respond(rt, up, ev[k].invocation_id, ASTER_UPCALL_STATUS_OK, allow, sizeof allow);
            } else if (ev[k].kind == ASTER_UPCALL_KIND_EXPOSE_HTTP) {
                aster_upcall_respond(rt, up, ev[k].invocation_id, ASTER_UPCALL_STATUS_OK, pong, sizeof pong);
            }
            aster_upcall_buffer_release(rt, up, ev[k].buffer);
        }
        size_t need = 0;
        int32_t rc = aster_http_response_wait(rt, response, 0, NULL, 0, &need);
        if (rc != ASTER_HTTP_BODY_TIMEOUT) return rc == ASTER_HTTP_BODY_BUFFER_TOO_SMALL;
    }
    return 0;
}

static void test_relay_stream(void) {
    printf("[streaming relay 4.3]\n");
    /* Relay codes continue BodyError's; the reserved auth statuses. */
    CHECK(ASTER_HTTP_BODY_UNKNOWN_SERVICE == 10 && ASTER_HTTP_BODY_DENIED == 11 &&
          ASTER_HTTP_BODY_OVERLOADED == 12 && ASTER_HTTP_BODY_RESET == 13 &&
          ASTER_HTTP_BODY_BAD_HEAD == 14 && ASTER_HTTP_BODY_CONNECTION_CLOSED == 15 &&
          ASTER_HTTP_BODY_UPLOAD_FAILED == 16 && ASTER_HTTP_BODY_BUFFER_TOO_SMALL == 17);
    CHECK(ASTER_STATUS_VERIFICATION_FAILED == 13 && ASTER_STATUS_UNAUTHENTICATED == 14 &&
          ASTER_STATUS_PERMISSION_DENIED == 15 && ASTER_STATUS_CREDENTIAL_EXPIRED == 16);
    CHECK(strcmp(aster_status_name(ASTER_STATUS_PERMISSION_DENIED), "ASTER_STATUS_PERMISSION_DENIED") == 0);
    CHECK(aster_http_body_status_to_status(ASTER_HTTP_BODY_DENIED) == ASTER_STATUS_PERMISSION_DENIED);
    CHECK(aster_http_body_status_to_status(ASTER_HTTP_BODY_UNKNOWN_SERVICE) == ASTER_STATUS_NOT_FOUND);
    CHECK(ASTER_EVENT_HTTP_RELAY_READY == 109);
    _Static_assert(sizeof(aster_http_upload_t) == 8 && sizeof(aster_http_response_t) == 8, "handle size");

    aster_http_relay_options_t o;
    memset(&o, 0xff, sizeof o);
    CHECK(aster_http_relay_options_init(&o) == ASTER_STATUS_OK);
    CHECK(o.struct_size == sizeof o && o.flags == 0 && o.max_chunk == 0 && o.upload_capacity == 0);

    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_bytes_t alpn = bstr("aster-expose/c-abi-test");
    aster_node_t server = relay_node(rt, &alpn);
    aster_node_t client = relay_node(rt, &alpn);
    CHECK(server != 0 && client != 0);

    /* Server: a foreign HTTP service "app", answered by this thread. */
    aster_upcalls_t up = 0;
    aster_http_bodies_t bodies = 0;
    aster_expose_t ex = 0;
    aster_operation_t serve_op = 0;
    CHECK(aster_upcalls_new(rt, 0, &up) == ASTER_STATUS_OK);
    CHECK(aster_http_bodies_new(rt, &bodies) == ASTER_STATUS_OK);
    CHECK(aster_expose_new(rt, server, &ex) == ASTER_STATUS_OK);
    CHECK(aster_expose_http_foreign(rt, ex, up, bodies, bstr("app"), NULL) == ASTER_STATUS_OK);
    CHECK(aster_expose_serve(rt, server, ex, (aster_bytes_list_t){&alpn, 1}, 0, &serve_op) == ASTER_STATUS_OK);

    /* Client: connect once the server has a loopback address. */
    uint8_t addr_buf[4096];
    aster_node_addr_t addr;
    memset(&addr, 0, sizeof addr);
    for (int i = 0; i < 100; i++) {
        CHECK(aster_node_addr_info(rt, server, addr_buf, sizeof addr_buf, &addr) == ASTER_STATUS_OK);
        if (addr.direct_addresses.len > 0) break;
        aster_event_t idle[1];
        aster_poll_events(rt, idle, 1, 50); /* a 50 ms pause (no op of ours is pending) */
    }
    CHECK(addr.direct_addresses.len > 0);
    aster_connect_config_t cc = {sizeof(aster_connect_config_t), 0, addr.endpoint_id, alpn, &addr};
    aster_operation_t op = 0;
    CHECK(aster_connect(rt, client, &cc, 0, &op) == ASTER_STATUS_OK);
    aster_event_t ev;
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev) && ev.kind == ASTER_EVENT_CONNECTED);
    aster_connection_t conn = ev.handle;

    /* C4 { "app", no peer, stream 0, HTTP/1.1, POST, "/up", no headers, Stream }. */
    const uint8_t c4[] = {0xC4, 0x01, 0x0E, 'a', 'p', 'p', 0xFD, 0x00, 0x02, 0x12, 'P', 'O', 'S', 'T',
                          0x0E, '/', 'u', 'p', 0x00, 0x0C, 0x01};
    aster_http_upload_t upload = 0;
    aster_http_response_t response = 0;
    CHECK(aster_expose_relay_stream(rt, conn, (aster_bytes_t){c4, sizeof c4}, &o, &upload, &response) ==
          ASTER_STATUS_OK);
    CHECK(upload != 0 && response != 0);
    CHECK(aster_http_upload_write(rt, upload, (const uint8_t *)"ping", 4, 5000) == ASTER_HTTP_BODY_OK);
    CHECK(aster_http_upload_finish(rt, upload) == ASTER_HTTP_BODY_OK);
    CHECK(aster_http_upload_finish(rt, upload) == ASTER_HTTP_BODY_FINISHED);

    /* The head: ask for its size, then fetch it (a UCv1 C5, body mode Stream). */
    CHECK(serve_until_head(rt, up, response));
    size_t need = 0, got = 0;
    CHECK(aster_http_response_wait(rt, response, 0, NULL, 0, &need) == ASTER_HTTP_BODY_BUFFER_TOO_SMALL);
    uint8_t head[256];
    CHECK(need > 4 && need <= sizeof head);
    CHECK(aster_http_response_wait(rt, response, 0, head, sizeof head, &got) == ASTER_HTTP_BODY_OK && got == need);
    CHECK(head[0] == 0xC5 && head[1] == 0x01 && head[2] == 0xC9 && head[3] == 0x01); /* status 201 */
    CHECK(head[got - 1] == 0x01);                                                /* body mode Stream */

    /* The body, then the sticky clean end. */
    uint8_t body[64];
    size_t n = 0;
    uint32_t eof = 0;
    char text[64] = {0};
    size_t total = 0;
    for (int i = 0; i < 10 && !eof; i++) {
        CHECK(aster_http_response_read(rt, response, body, sizeof body, 5000, &n, &eof) == ASTER_HTTP_BODY_OK);
        if (n > 0 && total + n < sizeof text) {
            memcpy(text + total, body, n);
            total += n;
        }
    }
    CHECK(eof == 1 && strcmp(text, "pong") == 0);
    CHECK(aster_http_response_read(rt, response, body, sizeof body, 0, &n, &eof) == ASTER_HTTP_BODY_OK && eof == 1);
    /* Readiness after the end fires at once: the next read succeeds (EOF). */
    CHECK(aster_http_response_ready(rt, response, 77, &op) == ASTER_STATUS_OK);
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev) && ev.kind == ASTER_EVENT_HTTP_RELAY_READY && ev.handle == response &&
          ev.status == ASTER_STATUS_OK && ev.error_code == 0 && ev.user_data == 77);
    CHECK(aster_http_upload_free(rt, upload) == ASTER_STATUS_OK);
    CHECK(aster_http_response_free(rt, response) == ASTER_STATUS_OK);
    CHECK(aster_http_response_free(rt, response) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_http_upload_write(rt, upload, (const uint8_t *)"x", 1, 0) == ASTER_HTTP_BODY_UNKNOWN_HANDLE);

    /* A service the peer does not expose: a stable code, not a message. */
    const uint8_t nope[] = {0xC4, 0x01, 0x12, 'n', 'o', 'p', 'e', 0xFD, 0x00, 0x02, 0x0E, 'G', 'E', 'T',
                            0x06, '/', 0x00, 0x0C, 0x01};
    CHECK(aster_expose_relay_stream(rt, conn, (aster_bytes_t){nope, sizeof nope}, NULL, &upload, &response) ==
          ASTER_STATUS_OK);
    CHECK(aster_http_upload_finish(rt, upload) == ASTER_HTTP_BODY_OK);
    CHECK(aster_http_response_wait(rt, response, 10000, head, sizeof head, &got) ==
          ASTER_HTTP_BODY_UNKNOWN_SERVICE);
    CHECK(aster_http_upload_free(rt, upload) == ASTER_STATUS_OK);
    CHECK(aster_http_response_free(rt, response) == ASTER_STATUS_OK);

    CHECK(aster_expose_free(rt, ex) == ASTER_STATUS_OK);
    CHECK(aster_node_free(rt, client) == ASTER_STATUS_OK);
    CHECK(aster_node_free(rt, server) == ASTER_STATUS_OK);
    CHECK(aster_upcalls_close(rt, up) == ASTER_STATUS_OK);
    CHECK(aster_upcalls_free(rt, up) == ASTER_STATUS_OK);
    CHECK(aster_http_bodies_free(rt, bodies) == ASTER_STATUS_OK);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

/* ABI 4.4: the coordination client's lifecycle, from C. No voter group is
 * reachable here (tests/coordination.rs has a live one): the journal lock,
 * request validation, an unreachable group's typed error and the handle
 * rules are what a binding can check without one. */
static void test_coordination(void) {
    printf("[coordination 4.4]\n");
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_config_t c;
    CHECK(aster_config_init(&c) == ASTER_STATUS_OK);
    c.relay_mode = ASTER_RELAY_MODE_DISABLED;
    c.bind_addr = (aster_bytes_t){(const uint8_t *)"127.0.0.1:0", 11};
    aster_operation_t op = 0;
    aster_event_t ev;
    CHECK(aster_node_start(rt, &c, 0, 0, &op) == ASTER_STATUS_OK);
    CHECK(wait_event(rt, op, &ev) && ev.kind == ASTER_EVENT_NODE_CREATED);
    aster_node_t node = ev.handle;

    /* A fresh journal directory (the client creates it) inside the runner's
     * scratch directory, which run-c-abi-test.sh removes. */
    char dir[512];
    const char *scratch = getenv("ASTER_ABI_TEST_DIR");
    snprintf(dir, sizeof dir, "%s/coord-%lx-%lx", scratch ? scratch : "/tmp",
             (unsigned long)time(NULL), (unsigned long)(uintptr_t)&dir);
    uint8_t group[32];
    memset(group, 0x5c, sizeof group);
    aster_coordination_client_options_t o;
    CHECK(aster_coordination_client_options_init(&o) == ASTER_STATUS_OK);
    CHECK(o.struct_size == sizeof o && o.flags == 0 && o.group_id == NULL);
    /* No group id: refused before anything starts. */
    CHECK(aster_coordination_client_open(rt, node, &o, 1, &op) == ASTER_STATUS_INVALID_ARGUMENT);
    o.group_id = group;
    o.journal_dir = bstr(dir);

    CHECK(aster_coordination_client_open(rt, node, &o, 1, &op) == ASTER_STATUS_OK);
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    CHECK(ev.kind == ASTER_EVENT_COORDINATION_CLIENT_OPENED && ev.handle != 0 && ev.related == node);
    aster_coordination_client_t client = ev.handle;

    /* The journal's lifetime lock: a second durable client is refused. */
    CHECK(aster_coordination_client_open(rt, node, &o, 2, &op) == ASTER_STATUS_OK);
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    CHECK(ev.kind == ASTER_EVENT_ERROR && ev.error_code == ASTER_COORDINATION_ERROR_JOURNAL_LOCKED);
    if (ev.buffer) aster_buffer_release(rt, ev.buffer);

    uint8_t gid[32] = {0}, local[32] = {0};
    CHECK(aster_coordination_client_ids(rt, client, gid, local) == ASTER_STATUS_OK);
    CHECK(memcmp(gid, group, 32) == 0);

    /* A malformed request never leaves the process. */
    uint8_t bad[] = {ASTER_COORDINATION_TAG_MUTATION_REQUEST, 0x01, 0};
    aster_bytes_t req = {bad, sizeof bad};
    CHECK(aster_coordination_mutate(rt, client, req, 0, &op) == ASTER_STATUS_INVALID_ARGUMENT);
    /* group_status: tag D1, version, op 5. No member is reachable. */
    uint8_t status_req[] = {ASTER_COORDINATION_TAG_READ_REQUEST, 0x01, ASTER_COORDINATION_READ_GROUP_STATUS};
    aster_bytes_t sreq = {status_req, sizeof status_req};
    CHECK(aster_coordination_read(rt, client, sreq, 3, &op) == ASTER_STATUS_OK);
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    CHECK(ev.kind == ASTER_EVENT_ERROR && ev.user_data == 3);
    CHECK(ev.error_code == ASTER_COORDINATION_ERROR_UNAVAILABLE && ev.status == ASTER_STATUS_CONNECTION_REFUSED);
    if (ev.buffer) aster_buffer_release(rt, ev.buffer);

    /* Close: the handle stays registered but refuses work; free twice. */
    CHECK(aster_coordination_client_close(rt, client) == ASTER_STATUS_OK);
    CHECK(aster_coordination_read(rt, client, sreq, 0, &op) == ASTER_STATUS_ALREADY_CLOSED);
    CHECK(aster_coordination_client_free(rt, client) == ASTER_STATUS_OK);
    CHECK(aster_coordination_client_free(rt, client) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_coordination_resolved_free(rt, 424242) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_coordination_lease_free(rt, 424242) == ASTER_STATUS_NOT_FOUND);

    /* The journal lock is released: reopen succeeds. */
    CHECK(aster_coordination_client_open(rt, node, &o, 4, &op) == ASTER_STATUS_OK);
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev) && ev.kind == ASTER_EVENT_COORDINATION_CLIENT_OPENED);
    CHECK(aster_coordination_client_free(rt, ev.handle) == ASTER_STATUS_OK);

    CHECK(aster_node_free(rt, node) == ASTER_STATUS_OK);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

/* ABI 4.6: observability (process-global) and the ops listener. Smoke for
 * every new symbol; behaviour is covered by ffi/tests/observability.rs and
 * ffi/tests/health.rs. */
static void test_observability(void) {
    printf("[observability 4.6]\n");
    aster_observability_config_t cfg;
    memset(&cfg, 0, sizeof cfg);
    cfg.struct_size = sizeof cfg;
    cfg.flags = ASTER_OBSERVABILITY_LOG_BRIDGE; /* no exporters, no env */
    cfg.service_name = (aster_bytes_t){(const uint8_t *)"abi-contract", 12};
    cfg.trace_sample_ratio = -1.0;
    cfg.binding = ASTER_BINDING_UNKNOWN;
    aster_observability_config_t bad = cfg;
    bad.struct_size = 8;
    CHECK(aster_observability_init(&bad) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_observability_init(&cfg) == ASTER_STATUS_OK);
    CHECK(aster_observability_init(&cfg) == ASTER_STATUS_OK); /* refcount 2 */
    aster_observability_config_t other = cfg;
    other.service_name = (aster_bytes_t){(const uint8_t *)"other", 5};
    CHECK(aster_observability_init(&other) == ASTER_STATUS_ALREADY_EXISTS);
    CHECK(aster_observability_flush(2000) == ASTER_STATUS_OK);

    aster_log_record_t recs[4];
    uint32_t nrecs = 7;
    CHECK(aster_log_poll(recs, 8, 4, 0, &nrecs) == ASTER_STATUS_INVALID_ARGUMENT && nrecs == 0); /* wrong stride */
    CHECK(aster_log_poll(recs, sizeof recs[0], 4, 0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
    nrecs = 7;
    CHECK(aster_log_poll(recs, sizeof recs[0], 4, 0, &nrecs) == ASTER_STATUS_OK && nrecs == 0); /* nothing queued */
    CHECK(aster_log_set_filter((const uint8_t *)"warn", 4) == ASTER_STATUS_OK);
    CHECK(aster_log_set_filter((const uint8_t *)"[[", 2) == ASTER_STATUS_INVALID_ARGUMENT);
    uint64_t dropped = 99;
    CHECK(aster_log_dropped(&dropped) == ASTER_STATUS_OK);
    CHECK(aster_log_dropped(NULL) == ASTER_STATUS_INVALID_ARGUMENT);
    aster_log_emit_t le;
    memset(&le, 0, sizeof le);
    le.struct_size = sizeof le;
    le.level = ASTER_LOG_INFO;
    le.logger = (aster_bytes_t){(const uint8_t *)"app", 3};
    le.message = (aster_bytes_t){(const uint8_t *)"hello", 5};
    CHECK(aster_log_emit(&le) == ASTER_STATUS_OK); /* no OTLP logs exporter: accepted, not exported */

    aster_instrument_desc_t d;
    memset(&d, 0, sizeof d);
    d.struct_size = sizeof d;
    d.kind = ASTER_INSTRUMENT_COUNTER_U64;
    d.name = (aster_bytes_t){(const uint8_t *)"abi.requests", 12};
    aster_instrument_t inst = 0;
    CHECK(aster_instrument_new(&d, &inst) == ASTER_STATUS_OK && inst != 0);
    d.kind = ASTER_INSTRUMENT_GAUGE_F64;
    {   /* histogram boundaries: strictly increasing, histograms only */
        static const double good[] = {1.0, 5.0};
        static const double bad[] = {5.0, 1.0};
        aster_instrument_desc_t hd = d;
        hd.kind = ASTER_INSTRUMENT_HISTOGRAM_F64;
        hd.name = (aster_bytes_t){(const uint8_t *)"abi.sizes", 9};
        hd.boundaries = bad; hd.boundaries_len = 2;
        aster_instrument_t hh = 0;
        CHECK(aster_instrument_new(&hd, &hh) == ASTER_STATUS_INVALID_ARGUMENT);
        hd.boundaries = good;
        CHECK(aster_instrument_new(&hd, &hh) == ASTER_STATUS_OK && hh != 0);
        CHECK(aster_metric_record_f64(hh, 3.0, 0, NULL, 0, 0) == ASTER_STATUS_OK);
        CHECK(aster_instrument_free(hh) == ASTER_STATUS_OK);
        aster_instrument_desc_t cd = d;   /* boundaries on a counter */
        cd.kind = ASTER_INSTRUMENT_COUNTER_U64;
        cd.name = (aster_bytes_t){(const uint8_t *)"abi.ctr2", 8};
        cd.boundaries = good; cd.boundaries_len = 2;
        aster_instrument_t ch = 0;
        CHECK(aster_instrument_new(&cd, &ch) == ASTER_STATUS_INVALID_ARGUMENT);
    }
    aster_instrument_t clash = 0;
    CHECK(aster_instrument_new(&d, &clash) == ASTER_STATUS_INVALID_ARGUMENT);
    static const uint8_t kv[] = {1, 0, 0, 0, 1, 0, 0, 0, 'k', 1, 0, 0, 0, 'v'};
    aster_attrset_t set = 0;
    CHECK(aster_attrset_new(kv, sizeof kv, &set) == ASTER_STATUS_OK && set != 0);
    CHECK(aster_metric_add_u64(inst, 3, set, NULL, 0, 0) == ASTER_STATUS_OK);
    CHECK(aster_metric_add_u64(inst, 1, 0, kv, sizeof kv, 0) == ASTER_STATUS_OK);
    CHECK(aster_metric_add_i64(inst, 1, 0, NULL, 0, 0) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_metric_record_f64(inst, 1.0, 0, NULL, 0, 0) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_metric_add_u64(424242, 1, 0, NULL, 0, 0) == ASTER_STATUS_NOT_FOUND);
    aster_bytes_t text = {0};
    CHECK(aster_metrics_prometheus(&text) == ASTER_STATUS_NOT_FOUND); /* no pull reader */
    CHECK(aster_attrset_free(set) == ASTER_STATUS_OK);
    CHECK(aster_instrument_free(inst) == ASTER_STATUS_OK);
    CHECK(aster_instrument_free(inst) == ASTER_STATUS_NOT_FOUND);

    aster_span_t span = 0;
    CHECK(aster_span_start((aster_bytes_t){(const uint8_t *)"work", 4}, ASTER_SPAN_INTERNAL, NULL, NULL, 0, &span) ==
          ASTER_STATUS_OK);
    CHECK(aster_span_set_attributes(span, kv, sizeof kv) == ASTER_STATUS_OK);
    CHECK(aster_span_add_event(span, (aster_bytes_t){(const uint8_t *)"e", 1}, NULL, 0) == ASTER_STATUS_OK);
    CHECK(aster_span_set_status(span, 2, (aster_bytes_t){(const uint8_t *)"bad", 3}) == ASTER_STATUS_OK);
    CHECK(aster_span_set_status(span, 9, (aster_bytes_t){0}) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_span_record_error(span, (aster_bytes_t){0}, (aster_bytes_t){(const uint8_t *)"x", 1}) ==
          ASTER_STATUS_OK);
    aster_span_context_t sc;
    CHECK(aster_span_context(span, &sc) == ASTER_STATUS_OK);
    CHECK(aster_span_end(span) == ASTER_STATUS_OK);
    CHECK(aster_span_end(span) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_span_start((aster_bytes_t){0}, 77, NULL, NULL, 0, &span) == ASTER_STATUS_INVALID_ARGUMENT);

    static const char tp[] = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";
    aster_span_context_t parsed;
    CHECK(aster_trace_context_parse((const uint8_t *)tp, 55, &parsed) == ASTER_STATUS_OK);
    CHECK(parsed.trace_flags == 1 && parsed.is_remote == 1);
    char buf[64];
    uint32_t n = 0;
    CHECK(aster_trace_context_format(&parsed, (uint8_t *)buf, sizeof buf, &n) == ASTER_STATUS_OK && n == 55);
    CHECK(memcmp(buf, tp, 55) == 0);
    CHECK(aster_trace_context_format(&parsed, (uint8_t *)buf, 10, &n) == ASTER_STATUS_BUFFER_TOO_SMALL);
    CHECK(aster_trace_context_parse((const uint8_t *)tp, 54, &parsed) == ASTER_STATUS_INVALID_ARGUMENT);
    aster_span_record_t rec;
    memset(&rec, 0, sizeof rec);
    rec.context = parsed;
    rec.start_unix_nanos = 1000;
    rec.end_unix_nanos = 2000;
    rec.name = (aster_bytes_t){(const uint8_t *)"recorded", 8};
    CHECK(aster_span_record(&rec) == ASTER_STATUS_OK);
    rec.end_unix_nanos = 10;
    CHECK(aster_span_record(&rec) == ASTER_STATUS_INVALID_ARGUMENT);

    CHECK(aster_log_close() == ASTER_STATUS_OK);
    CHECK(aster_observability_shutdown(5000) == ASTER_STATUS_OK);
    CHECK(aster_observability_shutdown(5000) == ASTER_STATUS_OK); /* last reference */
    CHECK(aster_observability_shutdown(5000) == ASTER_STATUS_ALREADY_CLOSED);
}

static void test_health(void) {
    printf("[health 4.6]\n");
    aster_runtime_t rt = 0;
    CHECK(aster_runtime_new(ASTER_ABI_VERSION_MAJOR, NULL, &rt) == ASTER_STATUS_OK);
    aster_config_t c;
    CHECK(aster_config_init(&c) == ASTER_STATUS_OK);
    c.relay_mode = ASTER_RELAY_MODE_DISABLED;
    c.bind_addr = (aster_bytes_t){(const uint8_t *)"127.0.0.1:0", 11};
    aster_bytes_t alpn = {(const uint8_t *)"aster/1", 7};
    c.alpns = (aster_bytes_list_t){&alpn, 1};
    aster_operation_t op = 0;
    CHECK(aster_node_start(rt, &c, 0, 9, &op) == ASTER_STATUS_OK);
    aster_event_t ev;
    memset(&ev, 0, sizeof ev);
    CHECK(wait_event(rt, op, &ev));
    aster_node_t node = ev.handle;

    aster_health_t h = 0;
    CHECK(aster_health_start(rt, node, NULL, &h) == ASTER_STATUS_INVALID_ARGUMENT); /* disabled in config */
    aster_health_config_t hc;
    memset(&hc, 0, sizeof hc);
    hc.struct_size = sizeof hc;
    hc.host = (aster_bytes_t){(const uint8_t *)"127.0.0.1", 9};
    CHECK(aster_health_start(rt, node, &hc, &h) == ASTER_STATUS_OK && h != 0);
    aster_bytes_t addr = {0};
    CHECK(aster_health_addr(h, &addr) == ASTER_STATUS_OK && addr.len > 10);
    CHECK(strncmp((const char *)addr.ptr, "127.0.0.1:", 10) == 0);
    CHECK(aster_string_release(addr.ptr, addr.len) == ASTER_STATUS_OK);
    aster_bytes_t text = {0};
    CHECK(aster_metrics_prometheus(&text) == ASTER_STATUS_OK && text.len > 0);
    CHECK(aster_string_release(text.ptr, text.len) == ASTER_STATUS_OK);

    aster_server_builder_t b = 0;
    CHECK(aster_server_builder_new(rt, node, &b) == ASTER_STATUS_OK);
    CHECK(aster_server_attach_health(b, h) == ASTER_STATUS_OK);
    CHECK(aster_server_attach_health(b, 424242) == ASTER_STATUS_NOT_FOUND);
    aster_server_t srv = 0;
    CHECK(aster_server_start(rt, b, 32, &srv) == ASTER_STATUS_OK);
    CHECK(aster_health_check_set(h, (aster_bytes_t){(const uint8_t *)"db", 2}, true) == ASTER_STATUS_OK);
    CHECK(aster_health_check_set(h, (aster_bytes_t){0}, true) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_health_set_healthy(h, true) == ASTER_STATUS_OK);
    CHECK(aster_health_set_ready(h, true) == ASTER_STATUS_OK);
    aster_span_context_t sc;
    CHECK(aster_dispatch_trace_context(rt, srv, 1, &sc) == ASTER_STATUS_NOT_FOUND);
    {
        const uint8_t *ip = NULL; uint32_t il = 0; aster_buffer_t ib = 0;
        CHECK(aster_dispatch_inbound_trace(rt, srv, 1, &ip, &il, &ib) == ASTER_STATUS_NOT_FOUND);
        CHECK(aster_dispatch_inbound_trace(rt, srv, 1, NULL, &il, &ib) == ASTER_STATUS_INVALID_ARGUMENT);
    }
    CHECK(aster_server_destroy(rt, srv) == ASTER_STATUS_OK);
    CHECK(aster_health_stop(rt, h) == ASTER_STATUS_OK);
    CHECK(aster_health_stop(rt, h) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_health_set_ready(h, true) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_node_free(rt, node) == ASTER_STATUS_OK);
    CHECK(aster_runtime_close(rt) == ASTER_STATUS_OK);
}

/* ABI 4.5: cancel tokens for aster_rpc_unary_cancellable (no runtime needed). */
static void test_cancel_tokens(void) {
    printf("[cancel tokens]\n");
    aster_cancel_token_t t = 0;
    CHECK(aster_cancel_token_new(0, NULL) == ASTER_STATUS_INVALID_ARGUMENT);
    CHECK(aster_cancel_token_new(0, &t) == ASTER_STATUS_OK && t != 0);
    CHECK(aster_cancel_token_cancel(0, t) == ASTER_STATUS_OK);
    CHECK(aster_cancel_token_cancel(0, t) == ASTER_STATUS_OK); /* idempotent */
    CHECK(aster_cancel_token_release(0, t) == ASTER_STATUS_OK);
    CHECK(aster_cancel_token_release(0, t) == ASTER_STATUS_NOT_FOUND);
    CHECK(aster_cancel_token_cancel(0, t) == ASTER_STATUS_NOT_FOUND);
}

int main(void) {
    test_abi_version();
    test_cancel_tokens();
    test_enum_values();
    test_runtime();
    test_envelope();
    test_config();
    test_gate_and_node();
    test_data_plane();
    test_pure_functions();
    test_dispatch_server();
    test_upcalls();
    test_expose();
    test_relay_stream();
    test_coordination();
    test_observability();
    test_health();
    if (failures) {
        printf("abi_contract_test: %d failure(s)\n", failures);
        return 1;
    }
    printf("abi_contract_test: all checks passed\n");
    return 0;
}
