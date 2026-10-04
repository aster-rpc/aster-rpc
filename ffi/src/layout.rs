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

use core::mem::{align_of, offset_of, size_of};

use crate::envelope::{aster_call_header_t, aster_rpc_status_t, aster_stream_header_t};
use crate::node::{aster_config_t, aster_portmapper_t, aster_trust_mode_t};
use crate::reactor::aster_reactor_call_t;
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

#[cfg(target_pointer_width = "64")]
mod lp64 {
    use super::*;

    assert_layout!(aster_bytes_t, size = 16, align = 8, { ptr = 0, len = 8 });
    assert_layout!(aster_bytes_list_t, size = 16, align = 8, { items = 0, len = 8 });

    assert_layout!(aster_runtime_config_t, size = 16, align = 4, {
        struct_size = 0,
        worker_threads = 4,
        event_queue_capacity = 8,
        reserved = 12,
    });

    assert_layout!(aster_endpoint_config_t, size = 152, align = 8, {
        struct_size = 0,
        relay_mode = 4,
        secret_key = 8,
        alpns = 24,
        relay_urls = 40,
        enable_discovery = 56,
        enable_hooks = 60,
        hook_timeout_ms = 64,
        bind_addr = 72,
        clear_ip_transports = 88,
        clear_relay_transports = 92,
        portmapper_config = 96,
        proxy_url = 104,
        proxy_from_env = 120,
        data_dir_utf8 = 128,
        hook_failure_mode = 144,
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

    assert_layout!(aster_reactor_call_t, size = 112, align = 8, {
        event_kind = 0,
        call_id = 8,
        connection_id = 16,
        stream_id = 24,
        header_ptr = 32,
        header_len = 40,
        header_flags = 44,
        request_ptr = 48,
        request_len = 56,
        request_flags = 60,
        peer_ptr = 64,
        peer_len = 72,
        header_buffer = 80,
        request_buffer = 88,
        peer_buffer = 96,
        connection_handle = 104,
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
}
