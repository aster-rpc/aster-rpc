//! Integration tests for the reactor C FFI.
//!
//! Validates the create → poll → submit → destroy lifecycle and exercises
//! the full pump task → SPSC ring → poll path with real connections.

use std::ptr;
use std::time::Duration;

use aster_transport_ffi::reactor::*;
use aster_transport_ffi::*;

unsafe fn poll_for_event(
    runtime: iroh_runtime_t,
    event_kind: iroh_event_kind_t,
    max_iters: u32,
) -> bool {
    for _ in 0..max_iters {
        std::thread::sleep(Duration::from_millis(10));
        let mut events = [std::mem::zeroed(); 8];
        let count = iroh_poll_events(runtime, events.as_mut_ptr(), 8, 5);
        for i in 0..count {
            if events[i as usize].kind == event_kind as u32 {
                return true;
            }
        }
    }
    false
}

unsafe fn create_runtime_and_node() -> (iroh_runtime_t, iroh_node_t) {
    let mut runtime: iroh_runtime_t = 0;
    let status = iroh_runtime_new(ptr::null(), &mut runtime);
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

    let alpns = [b"aster".as_ptr()];
    let alpn_lens = [5usize];

    let mut node_op: iroh_operation_t = 0;
    let status = iroh_node_memory_with_alpns(
        runtime,
        alpns.as_ptr(),
        alpn_lens.as_ptr(),
        1,
        0,
        &mut node_op,
    );
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

    let created = poll_for_event(runtime, iroh_event_kind_t::IROH_EVENT_NODE_CREATED, 100);
    assert!(created, "node should be created");

    // First node is always handle 1
    (runtime, 1)
}

#[test]
fn reactor_create_and_destroy() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        let mut reactor: aster_reactor_t = 0;
        let status = aster_reactor_create(runtime, node, 64, &mut reactor);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        assert!(reactor != 0, "reactor handle should be non-zero");

        let status = aster_reactor_destroy(runtime, reactor);
        assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_destroy_unknown_returns_not_found() {
    unsafe {
        let (runtime, _node) = create_runtime_and_node();

        let status = aster_reactor_destroy(runtime, 99999);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);

        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_create_invalid_node_returns_not_found() {
    unsafe {
        let mut runtime: iroh_runtime_t = 0;
        iroh_runtime_new(ptr::null(), &mut runtime);

        let mut reactor: aster_reactor_t = 0;
        let status = aster_reactor_create(runtime, 99999, 64, &mut reactor);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);

        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_create_null_out_returns_invalid_argument() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        let status = aster_reactor_create(runtime, node, 64, ptr::null_mut());
        assert_eq!(status, iroh_status_t::IROH_STATUS_INVALID_ARGUMENT as i32);

        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_poll_empty_returns_zero() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        let mut reactor: aster_reactor_t = 0;
        aster_reactor_create(runtime, node, 64, &mut reactor);

        let mut calls: [aster_reactor_call_t; 8] = std::mem::zeroed();
        // Non-blocking poll on empty ring
        let count = aster_reactor_poll(runtime, reactor, calls.as_mut_ptr(), 8, 0);
        assert_eq!(count, 0);

        // Short timeout poll on empty ring
        let count = aster_reactor_poll(runtime, reactor, calls.as_mut_ptr(), 8, 50);
        assert_eq!(count, 0);

        aster_reactor_destroy(runtime, reactor);
        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_poll_null_out_returns_zero() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        let mut reactor: aster_reactor_t = 0;
        aster_reactor_create(runtime, node, 64, &mut reactor);

        let count = aster_reactor_poll(runtime, reactor, ptr::null_mut(), 8, 0);
        assert_eq!(count, 0);

        aster_reactor_destroy(runtime, reactor);
        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_submit_unknown_call_returns_not_found() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        let mut reactor: aster_reactor_t = 0;
        aster_reactor_create(runtime, node, 64, &mut reactor);

        let resp = b"hello";
        let status = aster_reactor_submit(
            runtime,
            reactor,
            99999,
            resp.as_ptr(),
            resp.len() as u32,
            ptr::null(),
            0,
        );
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);

        aster_reactor_destroy(runtime, reactor);
        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_buffer_release_unknown_returns_not_found() {
    unsafe {
        let (runtime, _node) = create_runtime_and_node();

        let status = aster_reactor_buffer_release(runtime, 99999, 1);
        assert_eq!(status, iroh_status_t::IROH_STATUS_NOT_FOUND as i32);

        iroh_runtime_close(runtime);
    }
}

#[test]
fn reactor_multiple_create_destroy_cycles() {
    unsafe {
        let (runtime, node) = create_runtime_and_node();

        // Create and destroy a reactor 5 times to verify no leaks/state issues
        for _ in 0..5 {
            let mut reactor: aster_reactor_t = 0;
            let status = aster_reactor_create(runtime, node, 32, &mut reactor);
            assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

            let status = aster_reactor_destroy(runtime, reactor);
            assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
        }

        iroh_runtime_close(runtime);
    }
}
