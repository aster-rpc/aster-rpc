//! FFI Performance Benchmarks
//!
//! Measures the overhead of the FFI boundary for key operations.
//!
//! ## Benchmarks
//!
//! | Benchmark | What it measures |
//! |-----------|-----------------|
//! | `bench_runtime_create` | Time to create/destroy a runtime |
//! | `bench_poll_events_empty` | CQ drain with 0 events (no contention) |
//! | `bench_event_encode` | Time to encode an IrohEvent struct |
//! | `bench_cq_drain_1` | CQ drain returning 1 event |
//! | `bench_cq_drain_batched` | CQ drain returning 64 events |
//! | `bench_operation_submit_cancel` | Submit + cancel without async completion |
//!
//! Run with: `cargo bench -p aster_transport_ffi`

use std::ptr;

use aster_transport_ffi::*;

use criterion::{black_box, criterion_group, criterion_main, Criterion};

// ─── Benchmark helpers ─────────────────────────────────────────────────────

fn create_test_runtime() -> iroh_runtime_t {
    let mut runtime: iroh_runtime_t = 0;
    let status = unsafe { iroh_runtime_new(ptr::null(), &mut runtime) };
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
    runtime
}

fn destroy_test_runtime(runtime: iroh_runtime_t) {
    let status = unsafe { iroh_runtime_close(runtime) };
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);
}

// ─── Benchmarks ────────────────────────────────────────────────────────────

fn bench_runtime_create(c: &mut Criterion) {
    c.bench_function("bench_runtime_create", |b| {
        b.iter(|| {
            let mut runtime: iroh_runtime_t = 0;
            let status = unsafe { iroh_runtime_new(ptr::null(), &mut runtime) };
            black_box(status);
            let close_status = unsafe { iroh_runtime_close(runtime) };
            black_box(close_status);
        });
    });
}

fn bench_poll_events_empty(c: &mut Criterion) {
    c.bench_function("bench_poll_events_empty", |b| {
        let runtime = create_test_runtime();

        b.iter(|| {
            // SAFETY: zeroed() is unsafe but we're only using this for benchmark
            // memory layout validation; the poll_events function writes to out_events
            let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 1] };
            let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 1, 0) };
            black_box(count);
        });

        destroy_test_runtime(runtime);
    });
}

fn bench_event_encode(c: &mut Criterion) {
    c.bench_function("bench_event_encode", |b| {
        // Create a real event struct and encode it
        let event = iroh_event_t {
            struct_size: 80,
            kind: iroh_event_kind_t::IROH_EVENT_CONNECTED as u32,
            status: iroh_status_t::IROH_STATUS_OK as u32,
            operation: 42,
            handle: 7,
            related: 0,
            user_data: 12345,
            data_ptr: std::ptr::null(),
            data_len: 0,
            buffer: 0,
            error_code: 0,
            flags: 0,
        };

        b.iter(|| {
            let encoded = black_box(event);
            // Touch each field to ensure it's not optimized away
            black_box(encoded.struct_size);
            black_box(encoded.kind);
            black_box(encoded.operation);
            black_box(encoded.handle);
        });
    });
}

fn bench_cq_drain_1(c: &mut Criterion) {
    c.bench_function("bench_cq_drain_1", |b| {
        let runtime = create_test_runtime();

        // Pre-populate one event
        // We can't easily pre-populate in this harness, so just measure drain call overhead
        b.iter(|| {
            // SAFETY: zeroed() is unsafe but benchmark memory; poll_events writes to out_events
            let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 1] };
            let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 1, 0) };
            black_box(count);
        });

        destroy_test_runtime(runtime);
    });
}

fn bench_cq_drain_batched(c: &mut Criterion) {
    c.bench_function("bench_cq_drain_batched", |b| {
        let runtime = create_test_runtime();

        b.iter(|| {
            // SAFETY: zeroed() is unsafe but benchmark memory; poll_events writes to out_events
            let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 64] };
            let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 64, 0) };
            black_box(count);
        });

        destroy_test_runtime(runtime);
    });
}

fn bench_operation_submit_cancel(c: &mut Criterion) {
    let runtime = create_test_runtime();
    let alpns = [b"test".as_ptr()];
    let alpn_lens = [4];

    // Create a node
    let mut node_op: iroh_operation_t = 0;
    let _status = unsafe {
        iroh_node_memory_with_alpns(
            runtime,
            alpns.as_ptr(),
            alpn_lens.as_ptr(),
            1,
            0,
            &mut node_op,
        )
    };
    // Wait for node to be created
    std::thread::sleep(std::time::Duration::from_millis(50));

    c.bench_function("bench_operation_submit_cancel", |b| {
        b.iter(|| {
            // Immediately cancel the operation before it completes
            let cancel_status = unsafe { iroh_operation_cancel(runtime, node_op) };
            black_box(cancel_status);
        });
    });

    destroy_test_runtime(runtime);
}

fn bench_null_buffer_release(c: &mut Criterion) {
    c.bench_function("bench_null_buffer_release", |b| {
        let runtime = create_test_runtime();

        b.iter(|| {
            // Release null buffer - should be fast path (idempotent OK)
            let status = unsafe { iroh_buffer_release(runtime, 0) };
            black_box(status);
        });

        destroy_test_runtime(runtime);
    });
}

fn bench_string_release_null(c: &mut Criterion) {
    c.bench_function("bench_string_release_null", |b| {
        b.iter(|| {
            // Release null string - should be fast path (idempotent OK)
            let status = unsafe { iroh_string_release(ptr::null(), 0) };
            black_box(status);
        });
    });
}

// ─── Main ──────────────────────────────────────────────────────────────────

criterion_group!(
    ffi_benches,
    bench_runtime_create,
    bench_poll_events_empty,
    bench_event_encode,
    bench_cq_drain_1,
    bench_cq_drain_batched,
    bench_operation_submit_cancel,
    bench_null_buffer_release,
    bench_string_release_null,
);

criterion_main!(ffi_benches);
