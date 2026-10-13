//! Compile-time layout assertions for every `#[repr(C)]` type in the C ABI.
//!
//! These pin the sizes/offsets that `aster_ffi.h` (cbindgen output) promises
//! and that hand-written bindings (Java FFM layouts, Go, .NET) mirror. Any
//! change here is an ABI break and must bump `ASTER_ABI_VERSION_MAJOR`.
//! `ffi/tests/abi_layout_test.c` asserts the same numbers from the C side
//! against the generated header.
//!
//! Offsets are for LP64 / LLP64 64-bit targets (all supported platforms).

#![allow(dead_code)]

use crate::audit_outbox::aster_audit_outbox_error_t;
use core::mem::{align_of, offset_of, size_of};

use crate::coordination::{
    aster_coordination_client_options_t, aster_coordination_error_t,
    aster_coordination_mutation_op_t, aster_coordination_read_op_t,
};
use crate::data::{
    aster_blob_format_t, aster_blob_status_t, aster_fetch_strategy_t, aster_local_access_t,
    aster_path_info_t, aster_path_list_t, aster_retire_outcome_t,
};
use crate::envelope::{aster_call_header_t, aster_rpc_status_t, aster_stream_header_t};
use crate::expose::{
    aster_http_body_status_t, aster_http_foreign_options_t, aster_http_relay_options_t,
    aster_route_protocol_t, aster_route_spec_t,
};
use crate::grants::{
    aster_grant_context_t, aster_grant_outcome_kind_t, aster_grant_outcome_t, aster_grant_record_t,
    aster_grant_state_t,
};
use crate::health::aster_health_config_t;
use crate::http_server::aster_http_server_config_t;
use crate::node::{aster_config_t, aster_portmapper_t, aster_trust_mode_t};
use crate::observability::{
    aster_binding_kind_t, aster_instrument_desc_t, aster_instrument_kind_t, aster_log_emit_t,
    aster_log_level_t, aster_log_record_t, aster_observability_config_t, aster_otel_protocol_t,
    aster_span_context_t, aster_span_kind_t, aster_span_record_t,
};
use crate::pure::aster_id_kind_t;
use crate::raw_webtransport::aster_raw_wt_config_t;
use crate::rpc::aster_call_options_t;
use crate::server::{
    aster_cancel_reason_t, aster_dispatch_event_kind_t, aster_dispatch_event_t,
    aster_method_pattern_t, aster_server_option_t,
};
use crate::tls::aster_generated_cert_t;
use crate::upcall::{
    aster_lease_status_t, aster_topology_admitted_stats_t, aster_upcall_cancel_reason_t,
    aster_upcall_event_t, aster_upcall_kind_t, aster_upcall_status_t,
};
use crate::*;

macro_rules! assert_layout {
    ($ty:ty, size = $size:expr, align = $align:expr, { $($field:ident = $off:expr),* $(,)? }) => {
        const _: () = assert!(size_of::<$ty>() == $size, concat!("size_of ", stringify!($ty)));
        const _: () = assert!(align_of::<$ty>() == $align, concat!("align_of ", stringify!($ty)));
        $(
            const _: () = assert!(
                offset_of!($ty, $field) == $off,
                concat!("offset_of ", stringify!($ty), ".", stringify!($field))
            );
        )*
    };
}

// C enums are `int`-sized; every enum is carried as uint32_t/int32_t on the wire.
const _: () = assert!(size_of::<aster_status_t>() == 4);
const _: () = assert!(size_of::<aster_audit_outbox_error_t>() == 4);
const _: () = assert!(size_of::<aster_relay_mode_t>() == 4);
const _: () = assert!(size_of::<aster_hook_failure_mode_t>() == 4);
const _: () = assert!(size_of::<aster_hook_decision_t>() == 4);
const _: () = assert!(size_of::<aster_event_kind_t>() == 4);
const _: () = assert!(size_of::<aster_connection_type_t>() == 4);
const _: () = assert!(size_of::<aster_path_kind_t>() == 4);
const _: () = assert!(size_of::<aster_doc_share_mode_t>() == 4);
const _: () = assert!(size_of::<aster_registry_key_kind_t>() == 4);
const _: () = assert!(size_of::<aster_doc_query_mode_t>() == 4);
const _: () = assert!(size_of::<aster_download_policy_mode_t>() == 4);
const _: () = assert!(size_of::<aster_trust_mode_t>() == 4);
const _: () = assert!(size_of::<aster_portmapper_t>() == 4);
const _: () = assert!(size_of::<aster_blob_format_t>() == 4);
const _: () = assert!(size_of::<aster_fetch_strategy_t>() == 4);
const _: () = assert!(size_of::<aster_blob_status_t>() == 4);
const _: () = assert!(size_of::<aster_local_access_t>() == 4);
const _: () = assert!(size_of::<aster_retire_outcome_t>() == 4);
const _: () = assert!(size_of::<aster_id_kind_t>() == 4);
const _: () = assert!(size_of::<aster_grant_state_t>() == 4);
const _: () = assert!(size_of::<aster_grant_outcome_kind_t>() == 4);
const _: () = assert!(size_of::<aster_dispatch_event_kind_t>() == 4);
const _: () = assert!(size_of::<aster_cancel_reason_t>() == 4);
const _: () = assert!(size_of::<aster_method_pattern_t>() == 4);
const _: () = assert!(size_of::<aster_server_option_t>() == 4);
const _: () = assert!(size_of::<aster_upcall_kind_t>() == 4);
const _: () = assert!(size_of::<aster_upcall_status_t>() == 4);
const _: () = assert!(size_of::<aster_upcall_cancel_reason_t>() == 4);
const _: () = assert!(size_of::<aster_lease_status_t>() == 4);
const _: () = assert!(size_of::<aster_http_body_status_t>() == 4);
const _: () = assert!(size_of::<aster_route_protocol_t>() == 4);
const _: () = assert!(size_of::<aster_coordination_error_t>() == 4);
const _: () = assert!(size_of::<aster_coordination_mutation_op_t>() == 4);
const _: () = assert!(size_of::<aster_coordination_read_op_t>() == 4);
const _: () = assert!(size_of::<aster_binding_kind_t>() == 4);
const _: () = assert!(size_of::<aster_otel_protocol_t>() == 4);
const _: () = assert!(size_of::<aster_log_level_t>() == 4);
const _: () = assert!(size_of::<aster_instrument_kind_t>() == 4);
const _: () = assert!(size_of::<aster_span_kind_t>() == 4);

// ABI 4.6: platform-independent observability structs.
assert_layout!(aster_span_context_t, size = 32, align = 4, {
    trace_id = 0,
    span_id = 16,
    trace_flags = 24,
    is_remote = 25,
    _p0 = 26,
    _p1 = 28,
});

#[cfg(target_pointer_width = "64")]
mod lp64 {
    use super::*;

    assert_layout!(aster_bytes_t, size = 16, align = 8, { ptr = 0, len = 8 });
    assert_layout!(aster_bytes_list_t, size = 16, align = 8, { items = 0, len = 8 });

    assert_layout!(aster_raw_wt_config_t, size = 104, align = 8, {
        struct_size=0, flags=4, bind_addr=8, certificate_pem=24, private_key_pem=40, path=56,
        max_sessions=72, max_streams=76, max_datagrams=80, max_datagram_bytes=84, max_read_bytes=88,
        max_pending_auth=92, max_auth_payload_bytes=96, reserved=100,
    });

    assert_layout!(aster_runtime_config_t, size = 16, align = 4, {
        struct_size = 0,
        worker_threads = 4,
        event_queue_capacity = 8,
        max_inflight_operations = 12,
    });

    assert_layout!(aster_connect_config_t, size = 48, align = 8, {
        struct_size = 0,
        flags = 4,
        node_id = 8,
        alpn = 24,
        addr = 40,
    });

    assert_layout!(aster_node_addr_t, size = 48, align = 8, {
        endpoint_id = 0,
        relay_url = 16,
        direct_addresses = 32,
    });

    assert_layout!(aster_event_t, size = 80, align = 8, {
        struct_size = 0,
        kind = 4,
        status = 8,
        operation = 16,
        handle = 24,
        related = 32,
        user_data = 40,
        data_ptr = 48,
        data_len = 56,
        buffer = 64,
        error_code = 72,
        flags = 76,
    });

    assert_layout!(aster_remote_info_t, size = 72, align = 8, {
        struct_size = 0,
        node_id = 8,
        is_connected = 24,
        connection_type = 28,
        relay_url = 32,
        last_handshake_ns = 48,
        bytes_sent = 56,
        bytes_received = 64,
    });

    assert_layout!(aster_connection_info_t, size = 56, align = 8, {
        struct_size = 0,
        connection_type = 4,
        bytes_sent = 8,
        bytes_received = 16,
        rtt_ns = 24,
        alpn = 32,
        is_connected = 48,
    });

    assert_layout!(aster_transport_snapshot_t, size = 56, align = 8, {
        struct_size = 0,
        path_kind = 4,
        peer_host = 8,
        peer_port = 24,
        _pad = 26,
        relay_url = 32,
        rtt_micros = 48,
    });

    assert_layout!(aster_transport_metrics_t, size = 136, align = 8, {
        struct_size = 0,
        send_ipv4 = 8,
        send_ipv6 = 16,
        send_relay = 24,
        recv_data_ipv4 = 32,
        recv_data_ipv6 = 40,
        recv_data_relay = 48,
        recv_datagrams = 56,
        num_conns_direct = 64,
        num_conns_opened = 72,
        num_conns_closed = 80,
        paths_direct = 88,
        paths_relay = 96,
        holepunch_attempts = 104,
        relay_home_change = 112,
        net_reports = 120,
        net_reports_full = 128,
    });

    // ABI 3.1: AEv1 envelope structs (envelope.rs).
    assert_layout!(aster_stream_header_t, size = 96, align = 8, {
        struct_size = 0,
        version = 4,
        call_id = 8,
        session_id = 12,
        deadline = 16,
        serialization_mode = 18,
        reserved0 = 19,
        reserved1 = 20,
        service = 24,
        method = 40,
        metadata_keys = 56,
        metadata_values = 72,
        storage = 88,
    });

    assert_layout!(aster_call_header_t, size = 72, align = 8, {
        struct_size = 0,
        call_id = 4,
        deadline = 8,
        reserved0 = 10,
        reserved1 = 12,
        method = 16,
        metadata_keys = 32,
        metadata_values = 48,
        storage = 64,
    });

    assert_layout!(aster_rpc_status_t, size = 64, align = 8, {
        struct_size = 0,
        code = 4,
        message = 8,
        detail_keys = 24,
        detail_values = 40,
        storage = 56,
    });

    // ABI 3.2: node configuration (node.rs).
    assert_layout!(aster_config_t, size = 280, align = 8, {
        struct_size = 0,
        relay_mode = 4,
        relay_urls = 8,
        secret_key = 24,
        secret_key_file = 40,
        storage_path = 56,
        gc_interval_ms = 72,
        bind_addr = 80,
        alpns = 96,
        identity_file = 112,
        proxy_url = 128,
        hook_timeout_ms = 144,
        hook_failure_mode = 152,
        enable_hooks = 156,
        enable_monitoring = 160,
        local_discovery = 164,
        clear_ip_transports = 168,
        clear_relay_transports = 172,
        portmapper = 176,
        proxy_from_env = 180,
        transport_set = 184,
        transport_initial_mtu = 188,
        transport_send_fairness = 192,
        transport_enable_segmentation_offload = 196,
        transport_max_concurrent_bidi_streams = 200,
        transport_max_concurrent_uni_streams = 208,
        transport_stream_receive_window = 216,
        transport_receive_window = 224,
        transport_send_window = 232,
        transport_max_idle_timeout_ms = 240,
        transport_keep_alive_interval_ms = 248,
        transport_datagram_receive_buffer_size = 256,
        transport_datagram_send_buffer_size = 264,
        storage = 272,
    });

    // ABI 3.3: connection paths (data.rs).
    assert_layout!(aster_path_info_t, size = 72, align = 8, {
        struct_size = 0,
        path_kind = 4,
        is_selected = 8,
        peer_port = 12,
        _pad = 14,
        peer_host = 16,
        relay_url = 32,
        other = 48,
        rtt_micros = 64,
    });

    assert_layout!(aster_path_list_t, size = 32, align = 8, {
        struct_size = 0,
        _pad = 4,
        items = 8,
        len = 16,
        storage = 24,
    });
    // ABI 3.4: sealed grants (grants.rs).
    assert_layout!(aster_grant_context_t, size = 104, align = 8, {
        struct_size = 0,
        reserved = 4,
        app = 8,
        granter = 24,
        recipient = 40,
        resource = 56,
        path = 72,
        role = 88,
    });

    assert_layout!(aster_grant_record_t, size = 72, align = 8, {
        struct_size = 0,
        state = 4,
        context_version = 8,
        reserved = 12,
        role = 16,
        envelope = 32,
        reason = 48,
        storage = 64,
    });

    assert_layout!(aster_grant_outcome_t, size = 72, align = 8, {
        struct_size = 0,
        kind = 4,
        access = 8,
        drops_capability = 12,
        payload = 16,
        reason = 32,
        role = 48,
        storage = 64,
    });

    // ABI 3.5: dynamic dispatch.
    assert_layout!(aster_dispatch_event_t, size = 104, align = 8, {
        kind = 0,
        pattern = 1,
        reason = 2,
        flags = 4,
        call_id = 8,
        service_idx = 16,
        method_idx = 20,
        session_key = 24,
        connection_id = 32,
        connection_handle = 40,
        deadline_unix_ms = 48,
        wire_call_id = 56,
        session_id = 60,
        request_ptr = 64,
        request_len = 72,
        peer_ptr = 80,
        peer_len = 88,
        buffer = 96,
    });

    assert_layout!(aster_call_options_t, size = 72, align = 8, {
        struct_size = 0,
        version = 4,
        service = 8,
        method = 24,
        metadata = 40,
        session_id = 56,
        deadline_ms = 60,
        serialization_mode = 64,
        wire_call_id = 68,
    });

    // ABI 4.1: upcalls (upcall.rs).
    assert_layout!(aster_upcall_event_t, size = 40, align = 8, {
        invocation_id = 0,
        kind = 8,
        cancel_reason = 10,
        _p0 = 11,
        payload_len = 12,
        deadline_unix_ms = 16,
        payload_ptr = 24,
        buffer = 32,
    });

    assert_layout!(aster_topology_admitted_stats_t, size = 32, align = 8, {
        hits = 0,
        stale = 8,
        misses = 16,
        upcalls = 24,
    });

    // ABI 4.2: expose (expose.rs).
    assert_layout!(aster_http_foreign_options_t, size = 48, align = 8, {
        struct_size = 0,
        flags = 4,
        inline_request_max = 8,
        max_request_body = 16,
        max_response_body = 24,
        max_chunk = 32,
        response_buffer = 40,
        reserved = 44,
    });

    assert_layout!(aster_route_spec_t, size = 40, align = 8, {
        host = 0,
        service_id = 16,
        port = 32,
        protocol = 36,
    });

    // ABI 4.3: streaming consumer relay (expose.rs).
    assert_layout!(aster_http_relay_options_t, size = 24, align = 8, {
        struct_size = 0,
        flags = 4,
        max_chunk = 8,
        upload_capacity = 16,
        reserved = 20,
    });

    // ABI 4.4: coordination client (coordination.rs).
    assert_layout!(aster_coordination_client_options_t, size = 48, align = 8, {
        struct_size = 0,
        flags = 4,
        group_id = 8,
        seed_tickets = 16,
        journal_dir = 32,
    });

    // ABI 4.6: observability and health (observability.rs, health.rs).
    assert_layout!(aster_observability_config_t, size = 128, align = 8, {
        struct_size = 0,
        flags = 4,
        service_name = 8,
        otlp_endpoint = 24,
        otlp_headers = 40,
        resource_attrs = 56,
        log_filter = 72,
        extra_toml = 88,
        trace_sample_ratio = 104,
        protocol = 112,
        log_queue_capacity = 116,
        binding = 120,
        _reserved = 124,
    });

    assert_layout!(aster_log_record_t, size = 96, align = 8, {
        level = 0,
        flags = 1,
        _p0 = 2,
        _p1 = 4,
        timestamp_unix_ns = 8,
        trace_id = 16,
        span_id = 32,
        call_id = 40,
        target_ptr = 48,
        target_len = 56,
        message_ptr = 64,
        message_len = 72,
        fields_ptr = 80,
        fields_len = 88,
    });

    assert_layout!(aster_log_emit_t, size = 72, align = 8, {
        struct_size = 0,
        level = 4,
        timestamp_unix_ns = 8,
        logger = 16,
        message = 32,
        fields = 48,
        context = 64,
    });

    assert_layout!(aster_instrument_desc_t, size = 96, align = 8, {
        struct_size = 0,
        kind = 4,
        name = 8,
        description = 24,
        unit = 40,
        auto_attrs = 56,
        cardinality_limit = 72,
        _p = 76,
        boundaries = 80,
        boundaries_len = 88,
        _p1 = 92,
    });

    assert_layout!(aster_span_record_t, size = 152, align = 8, {
        context = 0,
        parent = 32,
        start_unix_nanos = 64,
        end_unix_nanos = 72,
        kind = 80,
        status_code = 84,
        name = 88,
        status_message = 104,
        kv = 120,
        kv_len = 128,
        events = 136,
        events_len = 144,
    });

    assert_layout!(aster_generated_cert_t, size = 64, align = 8, {
        cert_pem = 0, key_pem = 16, sha256 = 32,
    });
    assert_layout!(aster_http_server_config_t, size = 64, align = 8, {
        struct_size = 0, flags = 4, bind_addr = 8, cert_pem = 24,
        key_pem = 40, max_pending_auth = 56, max_auth_payload_bytes = 60,
    });

    assert_layout!(aster_health_config_t, size = 24, align = 8, {
        struct_size = 0,
        port = 4,
        _p = 6,
        host = 8,
    });
}
