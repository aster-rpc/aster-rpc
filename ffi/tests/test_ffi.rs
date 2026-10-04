//! Integration tests for aster_transport_ffi
//!
//! These tests validate the FFI layer by testing through the C-compatible API.

use std::ffi::CStr;
use std::ptr;
use std::time::Duration;

use aster_transport_ffi::*;

mod common;

#[test]
fn test_abi_version() {
    // 3.0.0: single `aster_` prefix, packed `aster_abi_version()`,
    // `aster_runtime_new(expected_abi_major, ...)`.
    // 3.1.0: additive AEv1 envelope + zstd functions (envelope.rs).
    assert_eq!(ASTER_ABI_VERSION_MAJOR, 3);
    // 3.2.0: additive node config, aster_node_start, Gate 0, admission (node.rs).
    // 3.3.0: additive data-plane parity (data.rs).
    // 3.4.0: additive pure functions: ids, HPKE, attestation, grants.
    assert_eq!(ASTER_ABI_VERSION_MINOR, 4);
    assert_eq!(ASTER_ABI_VERSION_PATCH, 0);
    let v = aster_abi_version();
    assert_eq!(v, ASTER_ABI_VERSION);
    assert_eq!(v >> 16, ASTER_ABI_VERSION_MAJOR);
    assert_eq!((v >> 8) & 0xff, ASTER_ABI_VERSION_MINOR);
    assert_eq!(v & 0xff, ASTER_ABI_VERSION_PATCH);
}

#[test]
fn test_runtime_new_rejects_abi_major_mismatch() {
    unsafe {
        for wrong in [0, ASTER_ABI_VERSION_MAJOR - 1, ASTER_ABI_VERSION_MAJOR + 1] {
            let mut runtime: aster_runtime_t = 0;
            let status = aster_runtime_new(wrong, ptr::null(), &mut runtime);
            assert_eq!(status, aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32);
            assert_eq!(runtime, 0, "no runtime may be created on mismatch");
        }
        let name = CStr::from_ptr(aster_status_name(
            aster_status_t::ASTER_STATUS_ABI_MISMATCH as i32,
        ));
        assert_eq!(name.to_str().unwrap(), "ASTER_STATUS_ABI_MISMATCH");
        let unknown = CStr::from_ptr(aster_status_name(-12345));
        assert_eq!(unknown.to_str().unwrap(), "ASTER_STATUS_UNKNOWN");
    }
}

#[test]
fn test_runtime_new_rejects_bad_struct_size() {
    unsafe {
        let config = aster_runtime_config_t {
            struct_size: 4,
            worker_threads: 1,
            event_queue_capacity: 16,
            reserved: 0,
        };
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
        assert_eq!(runtime, 0);
    }
}

#[test]
fn test_aster_build_version() {
    let version = unsafe { CStr::from_ptr(aster_version()) };
    assert_eq!(version.to_str().unwrap(), aster_transport_core::VERSION);
}

#[test]
fn test_status_names() {
    assert!(!aster_status_name(aster_status_t::ASTER_STATUS_OK as i32).is_null());
    assert!(!aster_status_name(aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32).is_null());
    assert!(!aster_status_name(aster_status_t::ASTER_STATUS_NOT_FOUND as i32).is_null());
    assert!(!aster_status_name(aster_status_t::ASTER_STATUS_INTERNAL as i32).is_null());
}

#[test]
fn test_runtime_create_and_close() {
    unsafe {
        let config = aster_runtime_config_t {
            struct_size: std::mem::size_of::<aster_runtime_config_t>() as u32,
            worker_threads: 2,
            event_queue_capacity: 100,
            reserved: 0,
        };

        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut runtime);

        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        assert!(runtime != 0, "Runtime handle should be non-zero");

        // Close the runtime
        let close_status = aster_runtime_close(runtime);
        assert_eq!(close_status, aster_status_t::ASTER_STATUS_OK as i32);
    }
}

#[test]
fn test_runtime_create_with_null_config() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        assert!(runtime != 0);

        // Clean up
        aster_runtime_close(runtime);
    }
}

#[test]
fn test_runtime_close_invalid_handle() {
    unsafe {
        let status = aster_runtime_close(999999);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);
    }
}

#[test]
fn test_runtime_close_null_out_param() {
    unsafe {
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
    }
}

#[test]
fn test_secret_key_generate() {
    unsafe {
        let mut key_buf = [0u8; 64];
        let mut len: usize = 0;

        let status = aster_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), &mut len);

        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        assert_eq!(len, 32, "Key should be 32 bytes");

        // Verify key is not all zeros (random)
        let all_zeros = key_buf[..32].iter().all(|&b| b == 0);
        assert!(!all_zeros, "Key should contain random data");
    }
}

#[test]
fn test_secret_key_generate_small_buffer() {
    unsafe {
        let mut key_buf = [0u8; 10]; // Too small
        let mut len: usize = 0;

        let status = aster_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), &mut len);

        assert_eq!(status, aster_status_t::ASTER_STATUS_BUFFER_TOO_SMALL as i32);
    }
}

#[test]
fn test_secret_key_generate_null_out_len() {
    unsafe {
        let mut key_buf = [0u8; 64];
        let status =
            aster_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), ptr::null_mut());

        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);
    }
}

#[test]
fn test_poll_events_no_events() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut events = [std::mem::zeroed(); 1];
        let count = aster_poll_events(runtime, events.as_mut_ptr(), 1, 0);

        assert_eq!(count, 0, "Should have no events initially");

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_poll_events_invalid_runtime() {
    unsafe {
        let mut events = [std::mem::zeroed(); 1];
        let count = aster_poll_events(999999, events.as_mut_ptr(), 1, 0);

        assert_eq!(count, 0, "Should return 0 for invalid runtime");
    }
}

#[test]
fn test_poll_events_null_out_events() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let count = aster_poll_events(runtime, ptr::null_mut(), 1, 0);

        assert_eq!(count, 0, "Should return 0 for null out_events");

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_buffer_release() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // Release buffer 0 should succeed
        let status = aster_buffer_release(runtime, 0);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // Release non-existent buffer should fail
        let status = aster_buffer_release(runtime, 999999);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_operation_cancel() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // Cancel invalid operation
        let status = aster_operation_cancel(runtime, 999999);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Cancel with invalid runtime
        let status = aster_operation_cancel(999999, 1);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Cancel with zero operation
        let status = aster_operation_cancel(runtime, 0);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_node_memory_creation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;
        let status = aster_node_memory(runtime, 42, &mut operation);

        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        assert!(operation != 0, "Operation handle should be non-zero");

        // Wait for event
        std::thread::sleep(Duration::from_millis(100));

        let mut events = [std::mem::zeroed(); 1];
        let count = aster_poll_events(runtime, events.as_mut_ptr(), 1, 100);

        // Poll completed without panicking/crashing.
        let _ = count;

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_node_close_null_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let status = aster_node_close(runtime, 1, 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_node_close_invalid_handle() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;
        let status = aster_node_close(runtime, 999999, 0, &mut operation);

        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_node_id_invalid_node() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut buf = [0u8; 64];
        let mut len: usize = 0;

        let status = aster_node_id(runtime, 999999, buf.as_mut_ptr(), buf.len(), &mut len);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_node_id_null_out_len() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut buf = [0u8; 64];
        let status = aster_node_id(runtime, 1, buf.as_mut_ptr(), buf.len(), ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_free_functions_on_invalid_handles() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // All these should return NOT_FOUND for invalid handles
        assert_eq!(
            aster_node_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_endpoint_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_connection_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_send_stream_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_recv_stream_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_doc_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );
        assert_eq!(
            aster_gossip_topic_free(runtime, 999999),
            aster_status_t::ASTER_STATUS_NOT_FOUND as i32
        );

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_endpoint_create_invalid_config() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;

        // Null config
        let status = aster_endpoint_create(runtime, ptr::null(), 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // Null out_operation
        let config = aster_endpoint_config_t {
            struct_size: std::mem::size_of::<aster_endpoint_config_t>() as u32,
            relay_mode: 0,
            secret_key: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            alpns: aster_bytes_list_t {
                items: ptr::null(),
                len: 0,
            },
            relay_urls: aster_bytes_list_t {
                items: ptr::null(),
                len: 0,
            },
            enable_discovery: 0,
            enable_hooks: 0,
            hook_timeout_ms: 0,
            bind_addr: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            clear_ip_transports: 0,
            clear_relay_transports: 0,
            portmapper_config: 0,
            proxy_url: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            proxy_from_env: 0,
            data_dir_utf8: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            hook_failure_mode: aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_OPEN as u32,
        };
        let status = aster_endpoint_create(runtime, &config, 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        let mut short_config = config;
        short_config.struct_size = 0;
        let status = aster_endpoint_create(runtime, &short_config, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connect_invalid_params() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;

        // Null config
        let status = aster_connect(runtime, 1, ptr::null(), 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // Invalid endpoint/node handle
        let config = aster_connect_config_t {
            struct_size: std::mem::size_of::<aster_connect_config_t>() as u32,
            flags: 0,
            node_id: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            alpn: aster_bytes_t {
                ptr: ptr::null(),
                len: 0,
            },
            addr: ptr::null(),
        };
        let status = aster_connect(runtime, 999999, &config, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_accept_invalid_params() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;

        // Invalid endpoint/node handle
        let status = aster_accept(runtime, 999999, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Null out_operation
        let status = aster_accept(runtime, 1, 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_stream_functions_invalid_handles() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;

        // Stream write with invalid handle
        let data = aster_bytes_t {
            ptr: b"hello".as_ptr(),
            len: 5,
        };
        let status = aster_stream_write(runtime, 999999, data, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Stream read with invalid handle
        let status = aster_stream_read(runtime, 999999, 1024, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Stream finish with invalid handle
        let status = aster_stream_finish(runtime, 999999, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_event_kinds_validity() {
    // Verify event kinds have expected values
    assert_eq!(aster_event_kind_t::ASTER_EVENT_NONE as u32, 0);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_NODE_CREATED as u32, 1);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_CONNECTED as u32, 10);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_STREAM_OPENED as u32, 20);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_BYTES_RESULT as u32, 91);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_ERROR as u32, 99);
}

#[test]
fn test_relay_mode_values() {
    assert_eq!(aster_relay_mode_t::ASTER_RELAY_MODE_DEFAULT as u32, 0);
    assert_eq!(aster_relay_mode_t::ASTER_RELAY_MODE_CUSTOM as u32, 1);
    assert_eq!(aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32, 2);
}

// ============================================================================
// Phase 1b: Datagram & Connection Info Tests
// ============================================================================

#[test]
fn test_connection_max_datagram_size_invalid_handle() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut out_size: u64 = 0;
        let mut out_is_some: u32 = 0;

        let status =
            aster_connection_max_datagram_size(runtime, 999999, &mut out_size, &mut out_is_some);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connection_max_datagram_size_null_params() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // Null out_size
        let status = aster_connection_max_datagram_size(runtime, 1, ptr::null_mut(), &mut 0);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // Null out_is_some
        let status = aster_connection_max_datagram_size(runtime, 1, &mut 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connection_datagram_send_buffer_space_invalid_handle() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut out_bytes: u64 = 0;

        let status = aster_connection_datagram_send_buffer_space(runtime, 999999, &mut out_bytes);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connection_datagram_send_buffer_space_null_param() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let status = aster_connection_datagram_send_buffer_space(runtime, 1, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connection_info_invalid_handle() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut out_info: aster_connection_info_t = std::mem::zeroed();

        let status = aster_connection_info(runtime, 999999, &mut out_info);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_connection_info_null_param() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let status = aster_connection_info(runtime, 1, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_endpoint_remote_info_invalid_params() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut out_info: aster_remote_info_t = std::mem::zeroed();

        // Invalid endpoint handle
        let node_id = aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        };
        let status = aster_endpoint_remote_info(runtime, 999999, node_id, &mut out_info);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Null out_info
        let status = aster_endpoint_remote_info(runtime, 1, node_id, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_endpoint_remote_info_list_invalid_params() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut out_infos: [aster_remote_info_t; 10] = [std::mem::zeroed(); 10];
        let mut out_count: usize = 0;

        // Invalid endpoint handle
        let status = aster_endpoint_remote_info_list(
            runtime,
            999999,
            out_infos.as_mut_ptr(),
            10,
            &mut out_count,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        // Null out_infos
        let status =
            aster_endpoint_remote_info_list(runtime, 1, ptr::null_mut(), 10, &mut out_count);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // Null out_count
        let status = aster_endpoint_remote_info_list(
            runtime,
            1,
            out_infos.as_mut_ptr(),
            10,
            ptr::null_mut(),
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // Zero max_infos
        let status =
            aster_endpoint_remote_info_list(runtime, 1, out_infos.as_mut_ptr(), 0, &mut out_count);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_hook_decision_values() {
    assert_eq!(aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32, 0);
    assert_eq!(aster_hook_decision_t::ASTER_HOOK_DECISION_DENY as u32, 1);
}

#[test]
fn test_hook_event_kinds() {
    assert_eq!(
        aster_event_kind_t::ASTER_EVENT_HOOK_BEFORE_CONNECT as u32,
        70
    );
    assert_eq!(
        aster_event_kind_t::ASTER_EVENT_HOOK_AFTER_CONNECT as u32,
        71
    );
    assert_eq!(
        aster_event_kind_t::ASTER_EVENT_HOOK_INVOCATION_RELEASED as u32,
        72
    );
}

#[test]
fn test_datagram_event_kinds() {
    assert_eq!(aster_event_kind_t::ASTER_EVENT_DATAGRAM_RECEIVED as u32, 60);
}

#[test]
fn test_connection_info_struct_size() {
    let expected_size = std::mem::size_of::<aster_connection_info_t>() as u32;
    assert!(
        expected_size > 0,
        "Connection info struct should have non-zero size"
    );

    // Verify struct fields are accessible — round-trip a few values.
    unsafe {
        let mut info: aster_connection_info_t = std::mem::zeroed();
        info.struct_size = expected_size;
        info.connection_type = 2; // UdpDirect
        info.is_connected = 1;
        assert_eq!(info.struct_size, expected_size);
        assert_eq!(info.connection_type, 2);
        assert_eq!(info.is_connected, 1);
    }
}

#[test]
fn test_remote_info_struct_size() {
    let expected_size = std::mem::size_of::<aster_remote_info_t>() as u32;
    assert!(
        expected_size > 0,
        "Remote info struct should have non-zero size"
    );

    // Verify struct fields are accessible — round-trip a few values.
    unsafe {
        let mut info: aster_remote_info_t = std::mem::zeroed();
        info.struct_size = expected_size;
        info.connection_type = 2; // UdpDirect
        info.is_connected = 1;
        assert_eq!(info.struct_size, expected_size);
        assert_eq!(info.connection_type, 2);
        assert_eq!(info.is_connected, 1);
    }
}

// ============================================================================
// Phase 1b: Hook Tests
// ============================================================================

#[test]
fn test_hook_endpoint_config_fields() {
    // Verify hooks fields are accessible in the config struct.
    let config = common::hooks_endpoint_config();
    assert_eq!(config.enable_hooks, 1);
    assert_eq!(config.hook_timeout_ms, 2000);
    assert_eq!(
        config.hook_failure_mode,
        aster_hook_failure_mode_t::ASTER_HOOK_FAILURE_FAIL_CLOSED as u32
    );
}

#[test]
fn test_hook_before_connect_respond_invalid_runtime() {
    unsafe {
        let status = aster_hook_before_connect_respond(
            999999,
            1,
            aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);
    }
}

#[test]
fn test_hook_after_connect_respond_invalid_runtime() {
    unsafe {
        let status = aster_hook_after_connect_respond(999999, 1);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);
    }
}

#[test]
fn test_hook_before_connect_respond_invalid_invocation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // No invocation with this ID exists — should be NOT_FOUND.
        let status = aster_hook_before_connect_respond(
            runtime,
            999999,
            aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_hook_after_connect_respond_invalid_invocation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        // No invocation with this ID exists — should be NOT_FOUND.
        let status = aster_hook_after_connect_respond(runtime, 999999);
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.5: Doc Sync Lifecycle FFI Tests
// ============================================================================

#[test]
fn test_doc_start_sync_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let peers = aster_bytes_list_t {
            items: ptr::null(),
            len: 0,
        };
        let status = aster_doc_start_sync(
            runtime,
            0,
            peers,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_start_sync_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let peers = aster_bytes_list_t {
            items: ptr::null(),
            len: 0,
        };
        let mut operation: aster_operation_t = 0;
        let status = aster_doc_start_sync(
            runtime,
            999999, // unknown doc
            peers,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_leave_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let status = aster_doc_leave(
            runtime,
            0,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_leave_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut operation: aster_operation_t = 0;
        let status = aster_doc_leave(
            runtime,
            999999, // unknown doc
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.4: Doc Subscribe FFI Tests
// ============================================================================

#[test]
fn test_doc_event_kind_values() {
    assert_eq!(aster_event_kind_t::ASTER_EVENT_DOC_SUBSCRIBED as u32, 47);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_DOC_EVENT as u32, 48);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_DOC_DEL as u32, 57);
}

#[test]
fn test_doc_del_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let bytes = aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        };
        let status = aster_doc_del(
            runtime,
            0,
            bytes,
            bytes,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_del_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let bytes = aster_bytes_t {
            ptr: ptr::null(),
            len: 0,
        };
        let mut operation: aster_operation_t = 0;
        let status = aster_doc_del(
            runtime,
            999999, // unknown doc
            bytes,
            bytes,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_subscribe_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let status = aster_doc_subscribe(
            runtime,
            0,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_subscribe_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut operation: aster_operation_t = 0;
        let status = aster_doc_subscribe(
            runtime,
            999999, // unknown doc
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_event_recv_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let status = aster_doc_event_recv(
            runtime,
            0,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_event_recv_unknown_receiver_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut operation: aster_operation_t = 0;
        let status = aster_doc_event_recv(
            runtime,
            999999, // unknown receiver handle
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.3: Blob Status / Has FFI Tests
// ============================================================================

#[test]
fn test_blobs_status_null_hash_ptr() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut out_status: u32 = 0;
        let mut out_size: u64 = 0;
        let status = aster_blobs_status(
            runtime,
            0,
            ptr::null(),
            0, // null hash_ptr -> INVALID_ARGUMENT
            &mut out_status,
            &mut out_size,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_status_null_out_status() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"aabbcc";
        let mut out_size: u64 = 0;
        let status = aster_blobs_status(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            ptr::null_mut(), // null out_status -> INVALID_ARGUMENT
            &mut out_size,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_status_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"aabbcc";
        let mut out_status: u32 = 0;
        let mut out_size: u64 = 0;
        let status = aster_blobs_status(
            runtime,
            999999, // unknown node
            hash.as_ptr(),
            hash.len(),
            &mut out_status,
            &mut out_size,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_has_null_hash_ptr() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut out_has: u32 = 0;
        let status = aster_blobs_has(
            runtime,
            0,
            ptr::null(),
            0, // null hash_ptr -> INVALID_ARGUMENT
            &mut out_has,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_has_null_out_has() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"aabbcc";
        let status = aster_blobs_has(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            ptr::null_mut(), // null out_has -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_has_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"aabbcc";
        let mut out_has: u32 = 0;
        let status = aster_blobs_has(
            runtime,
            999999, // unknown node
            hash.as_ptr(),
            hash.len(),
            &mut out_has,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c: Tag FFI Tests
// ============================================================================

#[test]
fn test_tag_event_kind_values() {
    // Ensure event kind discriminants match the documented ABI values.
    assert_eq!(aster_event_kind_t::ASTER_EVENT_TAG_SET as u32, 36);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_TAG_GET as u32, 37);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_TAG_DELETED as u32, 38);
    assert_eq!(aster_event_kind_t::ASTER_EVENT_TAG_LIST as u32, 39);
}

#[test]
fn test_tags_set_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let name = b"my-tag";
        let hash = b"aabbcc";
        let status = aster_tags_set(
            runtime,
            0,
            name.as_ptr(),
            name.len(),
            hash.as_ptr(),
            hash.len(),
            0,
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_set_null_name_ptr() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let hash = b"aabbcc";
        let mut operation: aster_operation_t = 0;
        let status = aster_tags_set(
            runtime,
            0,
            ptr::null(),
            0, // null name_ptr -> INVALID_ARGUMENT
            hash.as_ptr(),
            hash.len(),
            0,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_set_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let name = b"my-tag";
        let hash = b"aabbcc";
        let mut operation: aster_operation_t = 0;
        let status = aster_tags_set(
            runtime,
            999999, // unknown node
            name.as_ptr(),
            name.len(),
            hash.as_ptr(),
            hash.len(),
            0,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_get_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let name = b"my-tag";
        let status = aster_tags_get(
            runtime,
            0,
            name.as_ptr(),
            name.len(),
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_get_null_name_ptr() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let mut operation: aster_operation_t = 0;
        let status = aster_tags_get(
            runtime,
            0,
            ptr::null(),
            0, // null name_ptr -> INVALID_ARGUMENT
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_delete_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let name = b"my-tag";
        let status = aster_tags_delete(
            runtime,
            0,
            name.as_ptr(),
            name.len(),
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_tags_list_prefix_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let prefix = b"prefix/";
        let status = aster_tags_list_prefix(
            runtime,
            0,
            prefix.as_ptr(),
            prefix.len(),
            0,
            ptr::null_mut(), // null out_operation -> INVALID_ARGUMENT
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.6: Doc Download Policy FFI Tests
// ============================================================================

#[test]
fn test_download_policy_mode_constants() {
    assert_eq!(
        aster_download_policy_mode_t::ASTER_DOWNLOAD_POLICY_EVERYTHING as u32,
        0
    );
    assert_eq!(
        aster_download_policy_mode_t::ASTER_DOWNLOAD_POLICY_NOTHING_EXCEPT as u32,
        1
    );
    assert_eq!(
        aster_download_policy_mode_t::ASTER_DOWNLOAD_POLICY_EVERYTHING_EXCEPT as u32,
        2
    );
}

#[test]
fn test_doc_set_download_policy_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let empty_list = aster_bytes_list_t {
            items: ptr::null(),
            len: 0,
        };
        let status = aster_doc_set_download_policy(
            runtime,
            0,
            aster_download_policy_mode_t::ASTER_DOWNLOAD_POLICY_EVERYTHING as u32,
            empty_list,
            0,
            ptr::null_mut(),
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_set_download_policy_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let empty_list = aster_bytes_list_t {
            items: ptr::null(),
            len: 0,
        };
        let mut operation: aster_operation_t = 0;
        let status = aster_doc_set_download_policy(
            runtime,
            999999, // unknown doc handle
            aster_download_policy_mode_t::ASTER_DOWNLOAD_POLICY_EVERYTHING as u32,
            empty_list,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.7: Doc Share with Full Address FFI Tests
// ============================================================================

#[test]
fn test_doc_share_with_addr_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let status = aster_doc_share_with_addr(runtime, 0, 0, 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_doc_share_with_addr_unknown_doc_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let mut operation: aster_operation_t = 0;
        let status = aster_doc_share_with_addr(
            runtime,
            999999, // unknown doc handle
            0,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1c.8: Doc Join and Subscribe FFI Tests
// ============================================================================

#[test]
fn test_doc_join_and_subscribe_event_kind_value() {
    assert_eq!(
        aster_event_kind_t::ASTER_EVENT_DOC_JOINED_AND_SUBSCRIBED as u32,
        49
    );
}

#[test]
fn test_docs_join_and_subscribe_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let ticket_bytes = b"some-ticket";
        let ticket = aster_bytes_t {
            ptr: ticket_bytes.as_ptr(),
            len: ticket_bytes.len(),
        };
        let status = aster_docs_join_and_subscribe(runtime, 0, ticket, 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_docs_join_and_subscribe_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let ticket_bytes = b"some-ticket";
        let ticket = aster_bytes_t {
            ptr: ticket_bytes.as_ptr(),
            len: ticket_bytes.len(),
        };
        let mut operation: aster_operation_t = 0;
        let status = aster_docs_join_and_subscribe(
            runtime,
            999999, // unknown node
            ticket,
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_hook_endpoint_config_with_hooks_creates_operation() {
    // Verify that aster_endpoint_create with enable_hooks=1 accepts the config
    // and returns a valid operation handle. The actual hook drainer is async
    // and tested end-to-end through the Python layer.
    unsafe {
        let config = common::default_runtime_config();
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let ep_config = common::hooks_endpoint_config();
        let mut operation: aster_operation_t = 0;
        let status = aster_endpoint_create(runtime, &ep_config, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        assert!(operation != 0, "Operation handle should be non-zero");

        // Give the runtime a moment to process, then confirm hook responds
        // with unknown invocations still return NOT_FOUND.
        std::thread::sleep(Duration::from_millis(50));
        let status = aster_hook_before_connect_respond(
            runtime,
            999999,
            aster_hook_decision_t::ASTER_HOOK_DECISION_ALLOW as u32,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

// ============================================================================
// Phase 1d: Blob Transfer Observability FFI Tests
// ============================================================================

#[test]
fn test_blob_observe_complete_event_kind_value() {
    assert_eq!(
        aster_event_kind_t::ASTER_EVENT_BLOB_OBSERVE_COMPLETE as u32,
        56
    );
}

#[test]
fn test_blobs_observe_snapshot_null_out_args() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";
        let mut is_complete: u32 = 0;
        let mut size: u64 = 0;

        // null hash pointer
        let status =
            aster_blobs_observe_snapshot(runtime, 0, ptr::null(), 0, &mut is_complete, &mut size);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // null out_is_complete
        let status = aster_blobs_observe_snapshot(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            ptr::null_mut(),
            &mut size,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // null out_size
        let status = aster_blobs_observe_snapshot(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            &mut is_complete,
            ptr::null_mut(),
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_observe_snapshot_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";
        let mut is_complete: u32 = 0;
        let mut size: u64 = 0;

        let status = aster_blobs_observe_snapshot(
            runtime,
            999999, // unknown node
            hash.as_ptr(),
            hash.len(),
            &mut is_complete,
            &mut size,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_observe_complete_null_out_operation() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";

        // null hash pointer
        let mut operation: aster_operation_t = 0;
        let status = aster_blobs_observe_complete(runtime, 0, ptr::null(), 0, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // null out_operation
        let status =
            aster_blobs_observe_complete(runtime, 0, hash.as_ptr(), hash.len(), 0, ptr::null_mut());
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_observe_complete_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";
        let mut operation: aster_operation_t = 0;

        let status = aster_blobs_observe_complete(
            runtime,
            999999, // unknown node
            hash.as_ptr(),
            hash.len(),
            0,
            &mut operation,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_local_info_null_out_args() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";
        let mut is_complete: u32 = 0;
        let mut local_bytes: u64 = 0;

        // null hash pointer
        let status = aster_blobs_local_info(
            runtime,
            0,
            ptr::null(),
            0,
            &mut is_complete,
            &mut local_bytes,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // null out_is_complete
        let status = aster_blobs_local_info(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            ptr::null_mut(),
            &mut local_bytes,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        // null out_local_bytes
        let status = aster_blobs_local_info(
            runtime,
            0,
            hash.as_ptr(),
            hash.len(),
            &mut is_complete,
            ptr::null_mut(),
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_INVALID_ARGUMENT as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_blobs_local_info_unknown_node_returns_not_found() {
    unsafe {
        let mut runtime: aster_runtime_t = 0;
        aster_runtime_new(ASTER_ABI_VERSION_MAJOR, ptr::null(), &mut runtime);

        let hash = b"abcdefghijklmnopqrstuvwxyz012345678901234567890123";
        let mut is_complete: u32 = 0;
        let mut local_bytes: u64 = 0;

        let status = aster_blobs_local_info(
            runtime,
            999999, // unknown node
            hash.as_ptr(),
            hash.len(),
            &mut is_complete,
            &mut local_bytes,
        );
        assert_eq!(status, aster_status_t::ASTER_STATUS_NOT_FOUND as i32);

        aster_runtime_close(runtime);
    }
}

#[test]
fn test_endpoint_addr_info_direct_address_list_is_aligned() {
    // Regression: the `aster_bytes_t` array for `direct_addresses` is packed
    // after variable-length strings in the caller's buffer and used to be
    // written at an unaligned offset (UB; debug builds abort with
    // "misaligned pointer dereference"). Exercise every base alignment.
    unsafe {
        let config = common::default_runtime_config();
        let mut runtime: aster_runtime_t = 0;
        let status = aster_runtime_new(ASTER_ABI_VERSION_MAJOR, &config, &mut runtime);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);

        let ep_config = common::empty_endpoint_config();
        let mut operation: aster_operation_t = 0;
        let status = aster_endpoint_create(runtime, &ep_config, 0, &mut operation);
        assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
        let created = common::wait_for_successful_operation(
            runtime,
            operation,
            aster_event_kind_t::ASTER_EVENT_ENDPOINT_CREATED,
        );
        let endpoint = created.handle;

        let mut storage = vec![0u64; 1024];
        let base = storage.as_mut_ptr() as *mut u8;
        let mut saw_direct = false;
        for shift in 0..std::mem::align_of::<aster_bytes_t>() {
            let mut out: aster_node_addr_t = std::mem::zeroed();
            let status = aster_endpoint_addr_info(
                runtime,
                endpoint,
                base.add(shift),
                storage.len() * 8 - shift,
                &mut out,
            );
            assert_eq!(status, aster_status_t::ASTER_STATUS_OK as i32);
            assert!(out.endpoint_id.len > 0);
            if out.direct_addresses.len > 0 {
                saw_direct = true;
                let items = out.direct_addresses.items;
                assert_eq!(
                    items as usize % std::mem::align_of::<aster_bytes_t>(),
                    0,
                    "direct_addresses.items misaligned (shift {shift})"
                );
                for i in 0..out.direct_addresses.len {
                    let item = *items.add(i);
                    let s = std::slice::from_raw_parts(item.ptr, item.len);
                    assert!(std::str::from_utf8(s).is_ok());
                }
            }
        }
        if !saw_direct {
            eprintln!("note: endpoint reported no direct addresses; list alignment not exercised");
        }

        aster_runtime_close(runtime);
    }
}
