//! Common test utilities for aster_transport_ffi tests
//!
//! This module provides shared test utilities and helper functions.

#![allow(dead_code)]

/// Helper to create a default runtime config
#[cfg(test)]
pub fn default_runtime_config() -> aster_transport_ffi::iroh_runtime_config_t {
    aster_transport_ffi::iroh_runtime_config_t {
        struct_size: std::mem::size_of::<aster_transport_ffi::iroh_runtime_config_t>() as u32,
        worker_threads: 1,
        event_queue_capacity: 100,
        reserved: 0,
    }
}

/// Helper to create an empty endpoint config
#[cfg(test)]
pub fn empty_endpoint_config() -> aster_transport_ffi::iroh_endpoint_config_t {
    aster_transport_ffi::iroh_endpoint_config_t {
        struct_size: std::mem::size_of::<aster_transport_ffi::iroh_endpoint_config_t>() as u32,
        relay_mode: aster_transport_ffi::iroh_relay_mode_t::IROH_RELAY_MODE_DEFAULT as u32,
        secret_key: aster_transport_ffi::iroh_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        },
        alpns: aster_transport_ffi::iroh_bytes_list_t {
            items: std::ptr::null(),
            len: 0,
        },
        relay_urls: aster_transport_ffi::iroh_bytes_list_t {
            items: std::ptr::null(),
            len: 0,
        },
        enable_discovery: 0,
        enable_hooks: 0,
        hook_timeout_ms: 0,
        bind_addr: aster_transport_ffi::iroh_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        },
        clear_ip_transports: 0,
        clear_relay_transports: 0,
        portmapper_config: 0,
        proxy_url: aster_transport_ffi::iroh_bytes_t {
            ptr: std::ptr::null(),
            len: 0,
        },
        proxy_from_env: 0,
    }
}

/// Helper to create an endpoint config with hooks enabled
#[cfg(test)]
pub fn hooks_endpoint_config() -> aster_transport_ffi::iroh_endpoint_config_t {
    let mut cfg = empty_endpoint_config();
    cfg.enable_hooks = 1;
    cfg.hook_timeout_ms = 2000;
    cfg
}
