//! Integration tests for iroh_transport_ffi
//!
//! These tests validate the FFI layer by testing through the C-compatible API.

use std::ptr;
use std::time::Duration;

use iroh_transport_ffi::*;

mod common;

#[test]
fn test_abi_version() {
    assert_eq!(iroh_abi_version_major(), 1);
    assert_eq!(iroh_abi_version_minor(), 0);
    assert_eq!(iroh_abi_version_patch(), 0);
}

#[test]
fn test_status_names() {
    assert!(!iroh_status_name(iroh_status_t::IROH_STATUS_OK).is_null());
    assert!(!iroh_status_name(iroh_status_t::IROH_STATUS_INVALID_ARGUMENT).is_null());
    assert!(!iroh_status_name(iroh_status_t::IROH_STATUS_NOT_FOUND).is_null());
    assert!(!iroh_status_name(iroh_status_t::IROH_STATUS_INTERNAL).is_null());
}

#[test]
fn test_runtime_create_and_close() {
    unsafe {
        let config = iroh_runtime_config_t {
            struct_size: std::mem::size_of::<iroh_runtime_config_t>() as u32,
            worker_threads: 2,
            event_queue_capacity: 100,
            reserved: 0,
        };
        
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(&config, &mut runtime);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        assert!(runtime != 0, "Runtime handle should be non-zero");
        
        // Close the runtime
        let close_status = iroh_runtime_close(runtime);
        assert_eq!(close_status, iroh_status_t::IROH_STATUS_OK as i32);
    }
}

#[test]
fn test_runtime_create_with_null_config() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        assert!(runtime != 0);
        
        // Clean up
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_runtime_close_invalid_handle() {
    unsafe {
        let status = iroh_runtime_close(999999);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
    }
}

#[test]
fn test_runtime_close_null_out_param() {
    unsafe {
        let status = iroh_runtime_new(ptr::null(), ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
    }
}

#[test]
fn test_secret_key_generate() {
    unsafe {
        let mut key_buf = [0u8; 64];
        let mut len: usize = 0;
        
        let status = iroh_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), &mut len);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
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
        
        let status = iroh_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), &mut len);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_BUFFER_TOO_SMALL as i32);
    }
}

#[test]
fn test_secret_key_generate_null_out_len() {
    unsafe {
        let mut key_buf = [0u8; 64];
        let status = iroh_secret_key_generate(key_buf.as_mut_ptr(), key_buf.len(), ptr::null_mut());
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
    }
}

#[test]
fn test_poll_events_no_events() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut events = [std::mem::zeroed(); 1];
        let count = iroh_poll_events(runtime, events.as_mut_ptr(), 1, 0);
        
        assert_eq!(count, 0, "Should have no events initially");
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_poll_events_invalid_runtime() {
    unsafe {
        let mut events = [std::mem::zeroed(); 1];
        let count = iroh_poll_events(999999, events.as_mut_ptr(), 1, 0);
        
        assert_eq!(count, 0, "Should return 0 for invalid runtime");
    }
}

#[test]
fn test_poll_events_null_out_events() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let count = iroh_poll_events(runtime, ptr::null_mut(), 1, 0);
        
        assert_eq!(count, 0, "Should return 0 for null out_events");
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_buffer_release() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        // Release buffer 0 should succeed
        let status = iroh_buffer_release(runtime, 0);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        // Release non-existent buffer should fail
        let status = iroh_buffer_release(runtime, 999999);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_operation_cancel() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        // Cancel invalid operation
        let status = iroh_operation_cancel(runtime, 999999);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        // Cancel with invalid runtime
        let status = iroh_operation_cancel(999999, 1);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        // Cancel with zero operation
        let status = iroh_operation_cancel(runtime, 0);
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_node_memory_creation() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        let status = iroh_node_memory(runtime, 42, &mut operation);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        assert!(operation != 0, "Operation handle should be non-zero");
        
        // Wait for event
        std::thread::sleep(Duration::from_millis(100));
        
        let mut events = [std::mem::zeroed(); 1];
        let count = iroh_poll_events(runtime, events.as_mut_ptr(), 1, 100);
        
        // Poll completed without panicking/crashing.
        let _ = count;
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_node_close_null_operation() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let status = iroh_node_close(runtime, 1, 0, ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_node_close_invalid_handle() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        let status = iroh_node_close(runtime, 999999, 0, &mut operation);
        
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_node_id_invalid_node() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut buf = [0u8; 64];
        let mut len: usize = 0;
        
        let status = iroh_node_id(runtime, 999999, buf.as_mut_ptr(), buf.len(), &mut len);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_node_id_null_out_len() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut buf = [0u8; 64];
        let status = iroh_node_id(runtime, 1, buf.as_mut_ptr(), buf.len(), ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_free_functions_on_invalid_handles() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        // All these should return NOT_FOUND for invalid handles
        assert_eq!(iroh_node_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_endpoint_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_connection_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_send_stream_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_recv_stream_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_doc_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        assert_eq!(iroh_gossip_topic_free(runtime, 999999), iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_endpoint_create_invalid_config() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        
        // Null config
        let status = iroh_endpoint_create(runtime, ptr::null(), 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        // Null out_operation
        let config = iroh_endpoint_config_t {
            struct_size: std::mem::size_of::<iroh_endpoint_config_t>() as u32,
            relay_mode: 0,
            secret_key: iroh_bytes_t { ptr: ptr::null(), len: 0 },
            alpns: iroh_bytes_list_t { items: ptr::null(), len: 0 },
            relay_urls: iroh_bytes_list_t { items: ptr::null(), len: 0 },
            enable_discovery: 0,
            reserved: 0,
        };
        let status = iroh_endpoint_create(runtime, &config, 0, ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_connect_invalid_params() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        
        // Null config
        let status = iroh_connect(runtime, 1, ptr::null(), 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        // Invalid endpoint/node handle
        let config = iroh_connect_config_t {
            struct_size: std::mem::size_of::<iroh_connect_config_t>() as u32,
            flags: 0,
            node_id: iroh_bytes_t { ptr: ptr::null(), len: 0 },
            alpn: iroh_bytes_t { ptr: ptr::null(), len: 0 },
            addr: ptr::null(),
        };
        let status = iroh_connect(runtime, 999999, &config, 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_accept_invalid_params() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        
        // Invalid endpoint/node handle
        let status = iroh_accept(runtime, 999999, 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        // Null out_operation
        let status = iroh_accept(runtime, 1, 0, ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_stream_functions_invalid_handles() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        let status = iroh_runtime_new(ptr::null(), &mut runtime);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        
        let mut operation: iroh_operation_t = 0;
        
        // Stream write with invalid handle
        let data = iroh_bytes_t {
            ptr: b"hello".as_ptr(),
            len: 5,
        };
        let status = iroh_stream_write(runtime, 999999, data, 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        // Stream read with invalid handle
        let status = iroh_stream_read(runtime, 999999, 1024, 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        // Stream finish with invalid handle
        let status = iroh_stream_finish(runtime, 999999, 0, &mut operation);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);
        
        iroh_runtime_close(runtime);
    }
}

#[test]
fn test_event_kinds_validity() {
    // Verify event kinds have expected values
    assert_eq!(iroh_event_kind_t::IROH_EVENT_NONE as u32, 0);
    assert_eq!(iroh_event_kind_t::IROH_EVENT_NODE_CREATED as u32, 1);
    assert_eq!(iroh_event_kind_t::IROH_EVENT_CONNECTED as u32, 10);
    assert_eq!(iroh_event_kind_t::IROH_EVENT_STREAM_OPENED as u32, 20);
    assert_eq!(iroh_event_kind_t::IROH_EVENT_BYTES_RESULT as u32, 91);
    assert_eq!(iroh_event_kind_t::IROH_EVENT_ERROR as u32, 99);
}

#[test]
fn test_relay_mode_values() {
    assert_eq!(iroh_relay_mode_t::IROH_RELAY_MODE_DEFAULT as u32, 0);
    assert_eq!(iroh_relay_mode_t::IROH_RELAY_MODE_CUSTOM as u32, 1);
    assert_eq!(iroh_relay_mode_t::IROH_RELAY_MODE_DISABLED as u32, 2);
}
