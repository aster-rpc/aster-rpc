//! Common test utilities for aster_transport_ffi tests
//!
//! This module provides shared test utilities and helper functions.

#![allow(dead_code)]

use std::time::{Duration, Instant};
use std::{collections::HashSet, iter::FromIterator};

use aster_transport_ffi::{
    aster_buffer_release, aster_event_kind_t, aster_event_t, aster_operation_t, aster_poll_events,
    aster_runtime_t, aster_status_t,
};

/// Wait for one asynchronous FFI operation to produce its terminal event.
///
/// The old integration-test helpers slept in 10 ms increments and gave node
/// creation a one-second wall-clock budget. Under compiler or CI load that
/// expired while the operation was still healthy, and the event matching was
/// by kind rather than operation id. This helper blocks on the completion
/// queue in bounded slices, correlates the exact operation, and retains a
/// finite failure deadline without turning ordinary host load into a failure.
pub unsafe fn wait_for_operation(
    runtime: aster_runtime_t,
    operation: aster_operation_t,
) -> aster_event_t {
    const DEADLINE: Duration = Duration::from_secs(30);
    const POLL_SLICE: Duration = Duration::from_millis(250);

    let started = Instant::now();
    loop {
        let elapsed = started.elapsed();
        assert!(
            elapsed < DEADLINE,
            "operation {operation} did not produce a terminal event within {DEADLINE:?}"
        );
        let remaining = DEADLINE - elapsed;
        let timeout_ms = remaining
            .min(POLL_SLICE)
            .as_millis()
            .max(1)
            .try_into()
            .expect("poll slice fits u32");

        let mut events: [aster_event_t; 8] = std::mem::zeroed();
        let count = aster_poll_events(runtime, events.as_mut_ptr(), events.len(), timeout_ms);
        for event in events.iter().take(count) {
            if event.operation == operation {
                return *event;
            }

            // The event has been removed from the queue even though it is not
            // ours. Do not leak a payload while waiting for the correlated op.
            if event.buffer != 0 {
                let _ = aster_buffer_release(runtime, event.buffer);
            }
        }
    }
}

/// Wait for one operation and require its expected successful event.
pub unsafe fn wait_for_successful_operation(
    runtime: aster_runtime_t,
    operation: aster_operation_t,
    expected_kind: aster_event_kind_t,
) -> aster_event_t {
    let event = wait_for_operation(runtime, operation);
    assert_eq!(
        event.kind, expected_kind as u32,
        "operation {operation} produced event kind {} instead of {} (status {}, error {})",
        event.kind, expected_kind as u32, event.status, event.error_code
    );
    assert_eq!(
        event.status,
        aster_status_t::ASTER_STATUS_OK as u32,
        "operation {operation} failed with status {} (event kind {}, error {})",
        event.status,
        event.kind,
        event.error_code
    );
    event
}

/// Wait for a set of concurrently submitted operations, requiring one exact
/// successful event per operation.
///
/// This is intentionally stronger than sleeping and asserting that "some"
/// events arrived: the hostile-race CQ test is meant to prove that sharing a
/// runtime loses and duplicates none of them.
pub unsafe fn wait_for_successful_operations(
    runtime: aster_runtime_t,
    operations: &[aster_operation_t],
    expected_kind: aster_event_kind_t,
) -> Vec<aster_event_t> {
    const DEADLINE: Duration = Duration::from_secs(30);
    const POLL_SLICE: Duration = Duration::from_millis(250);

    let all = HashSet::<aster_operation_t>::from_iter(operations.iter().copied());
    assert_eq!(all.len(), operations.len(), "operation ids must be unique");
    let mut pending = all.clone();
    let mut completed = Vec::with_capacity(operations.len());
    let started = Instant::now();

    while !pending.is_empty() {
        let elapsed = started.elapsed();
        assert!(
            elapsed < DEADLINE,
            "{} of {} operations did not complete within {DEADLINE:?}: {pending:?}",
            pending.len(),
            operations.len()
        );
        let remaining = DEADLINE - elapsed;
        let timeout_ms = remaining
            .min(POLL_SLICE)
            .as_millis()
            .max(1)
            .try_into()
            .expect("poll slice fits u32");

        let mut events: [aster_event_t; 32] = std::mem::zeroed();
        let count = aster_poll_events(runtime, events.as_mut_ptr(), events.len(), timeout_ms);
        for event in events.iter().take(count) {
            if all.contains(&event.operation) {
                assert!(
                    pending.remove(&event.operation),
                    "operation {} produced more than one terminal event",
                    event.operation
                );
                assert_eq!(
                    event.kind,
                    expected_kind as u32,
                    "operation {} produced event kind {} instead of {} (status {}, error {})",
                    event.operation,
                    event.kind,
                    expected_kind as u32,
                    event.status,
                    event.error_code
                );
                assert_eq!(
                    event.status,
                    aster_status_t::ASTER_STATUS_OK as u32,
                    "operation {} failed with status {} (event kind {}, error {})",
                    event.operation,
                    event.status,
                    event.kind,
                    event.error_code
                );
                completed.push(*event);
            } else if event.buffer != 0 {
                let _ = aster_buffer_release(runtime, event.buffer);
            }
        }
    }

    completed
}

/// Helper to create a default runtime config
#[cfg(test)]
pub fn default_runtime_config() -> aster_transport_ffi::aster_runtime_config_t {
    aster_transport_ffi::aster_runtime_config_t {
        struct_size: std::mem::size_of::<aster_transport_ffi::aster_runtime_config_t>() as u32,
        worker_threads: 1,
        event_queue_capacity: 100,
        reserved: 0,
    }
}

/// Start an in-memory node (`aster_node_start`) serving `alpns`, relays off,
/// bound to loopback. Returns the status; the node arrives as
/// `ASTER_EVENT_NODE_CREATED` for `*out_op`. The config is copied before
/// `aster_node_start` returns, so the locals here may go away.
pub unsafe fn node_start_with_alpns(
    runtime: aster_runtime_t,
    alpns: &[&[u8]],
    user_data: u64,
    out_op: *mut aster_operation_t,
) -> i32 {
    node_start_with(runtime, alpns, false, user_data, out_op)
}

/// As `node_start_with_alpns`, optionally with admission hooks on.
pub unsafe fn node_start_with(
    runtime: aster_runtime_t,
    alpns: &[&[u8]],
    enable_hooks: bool,
    user_data: u64,
    out_op: *mut aster_operation_t,
) -> i32 {
    use aster_transport_ffi::node::{aster_config_init, aster_config_t, aster_node_start};
    use aster_transport_ffi::{aster_bytes_list_t, aster_bytes_t, aster_relay_mode_t};
    let items: Vec<aster_bytes_t> = alpns
        .iter()
        .map(|a| aster_bytes_t {
            ptr: a.as_ptr(),
            len: a.len(),
        })
        .collect();
    let bind = b"127.0.0.1:0";
    let mut cfg: aster_config_t = std::mem::zeroed();
    assert_eq!(aster_config_init(&mut cfg), 0);
    cfg.relay_mode = aster_relay_mode_t::ASTER_RELAY_MODE_DISABLED as u32;
    cfg.bind_addr = aster_bytes_t {
        ptr: bind.as_ptr(),
        len: bind.len(),
    };
    cfg.alpns = aster_bytes_list_t {
        items: items.as_ptr(),
        len: items.len(),
    };
    cfg.enable_hooks = u32::from(enable_hooks);
    aster_node_start(runtime, &cfg, 0, user_data, out_op)
}

/// Start an in-memory node serving `alpns` and wait for it.
pub unsafe fn start_node(runtime: aster_runtime_t, alpns: &[&[u8]]) -> u64 {
    let mut op = 0;
    assert_eq!(node_start_with_alpns(runtime, alpns, 0, &mut op), 0);
    wait_for_successful_operation(runtime, op, aster_event_kind_t::ASTER_EVENT_NODE_CREATED).handle
}
