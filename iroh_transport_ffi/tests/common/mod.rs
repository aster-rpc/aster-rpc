//! Common test utilities for iroh_transport_ffi tests
//!
//! This module provides shared test utilities and helper functions.

/// Helper to create a default runtime config
#[cfg(test)]
pub fn default_runtime_config() -> iroh_transport_ffi::iroh_runtime_config_t {
    iroh_transport_ffi::iroh_runtime_config_t {
        struct_size: std::mem::size_of::<iroh_transport_ffi::iroh_runtime_config_t>() as u32,
        worker_threads: 1,
        event_queue_capacity: 100,
        reserved: 0,
    }
}

/// Helper to create an empty endpoint config
#[cfg(test)]
pub fn empty_endpoint_config() -> iroh_transport_ffi::iroh_endpoint_config_t {
    iroh_transport_ffi::iroh_endpoint_config_t {
        struct_size: std::mem::size_of::<iroh_transport_ffi::iroh_endpoint_config_t>() as u32,
        relay_mode: iroh_transport_ffi::iroh_relay_mode_t::IROH_RELAY_MODE_DEFAULT as u32,
        secret_key: iroh_transport_ffi::iroh_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        },
        alpns: iroh_transport_ffi::iroh_bytes_list_t {
            items: std::ptr::null(),
            len: 0,
        },
        relay_urls: iroh_transport_ffi::iroh_bytes_list_t {
            items: std::ptr::null(),
            len: 0,
        },
        enable_discovery: 0,
        reserved: 0,
    }
}
