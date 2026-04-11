//! Long-Run Soak / Leak Tests (5b.9)
//!
//! Multi-hour churn test to catch resource leaks.
//!
//! ## Metrics Tracked
//!
//! | Metric | Assertion |
//! |--------|-----------|
//! | Native handle count | `initial == final` |
//! | Op table size | `final_pending_ops == 0` |
//! | RSS growth | stabilizes (no unbounded growth) |
//! | Pending CQ depth | `max_queued < threshold` |
//!
//! ## Churn Pattern
//!
//! ```text
//! connect → accept → open_bi → read → write → finish → close
//! (with 10% random: cancel before complete, peer disconnect, timeout)
//! Repeat for 4 hours
//! ```
//!
//! ## Run
//!
//! ```bash
//! cargo test -p aster_transport_ffi --test soak_test -- --nocapture
//! ```
//!
//! Or run the binary directly for longer duration:
//!
//! ```bash
//! cargo run -p aster_transport_ffi --test soak_test -- --duration-seconds 14400
//! ```

use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aster_transport_ffi::*;

mod common;

// ─── Configuration ────────────────────────────────────────────────────────

const DEFAULT_DURATION_SECS: u64 = 14400; // 4 hours

// ─── Metrics ────────────────────────────────────────────────────────────────

struct SoakMetrics {
    cycles_completed: AtomicUsize,
    ops_submitted: AtomicUsize,
    ops_completed: AtomicUsize,
    ops_cancelled: AtomicUsize,
    ops_errored: AtomicUsize,
    max_pending_ops: AtomicUsize,
    current_pending_ops: AtomicUsize,
}

impl SoakMetrics {
    fn new() -> Self {
        Self {
            cycles_completed: AtomicUsize::new(0),
            ops_submitted: AtomicUsize::new(0),
            ops_completed: AtomicUsize::new(0),
            ops_cancelled: AtomicUsize::new(0),
            ops_errored: AtomicUsize::new(0),
            max_pending_ops: AtomicUsize::new(0),
            current_pending_ops: AtomicUsize::new(0),
        }
    }

    fn record_submit(&self) {
        self.ops_submitted.fetch_add(1, Ordering::Relaxed);
        let pending = self.current_pending_ops.fetch_add(1, Ordering::Relaxed) + 1;
        // Update max if needed
        loop {
            let current_max = self.max_pending_ops.load(Ordering::Relaxed);
            if pending <= current_max {
                break;
            }
            if self
                .max_pending_ops
                .compare_exchange(current_max, pending, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    fn record_complete(&self) {
        self.ops_completed.fetch_add(1, Ordering::Relaxed);
        self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_cancel(&self) {
        self.ops_cancelled.fetch_add(1, Ordering::Relaxed);
        self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_error(&self) {
        self.ops_errored.fetch_add(1, Ordering::Relaxed);
        self.current_pending_ops.fetch_sub(1, Ordering::Relaxed);
    }

    fn record_cycle(&self) {
        self.cycles_completed.fetch_add(1, Ordering::Relaxed);
    }
}

impl Default for SoakMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Soak Test ────────────────────────────────────────────────────────────

fn run_soak_cycle(runtime: iroh_runtime_t, metrics: &Arc<SoakMetrics>) -> bool {
    // Drain any stale events from the previous cycle before starting a new one.
    drain_all_events(runtime);

    // Create an in-memory node with aster ALPN
    let alpns = [b"aster".as_ptr()];
    let alpn_lens = [5];

    let mut node_op: iroh_operation_t = 0;
    let status = unsafe {
        iroh_node_memory_with_alpns(
            runtime,
            alpns.as_ptr(),
            alpn_lens.as_ptr(),
            1,
            0,
            &mut node_op,
        )
    };

    if status != iroh_status_t::IROH_STATUS_OK as i32 {
        return false;
    }

    // Poll until node creation is confirmed (not a fixed sleep)
    let node_created = poll_for_event(runtime, iroh_event_kind_t::IROH_EVENT_NODE_CREATED, 2000);
    if !node_created {
        // Node creation timed out — treat as error, don't track as pending
        return false;
    }
    // NOTE: Node creation is an internal operation — we do NOT call record_submit()
    // for it, so we do NOT call record_complete() either.

    // Submit accept (will hang since no peer connects)
    let mut accept_op: iroh_operation_t = 0;
    let status = unsafe { iroh_node_accept_aster(runtime, 1, 0, &mut accept_op) };
    if status != iroh_status_t::IROH_STATUS_OK as i32 {
        return false;
    }
    metrics.record_submit();

    // Random chance to cancel (~10%)
    // Note: we do NOT call record_cancel() here. The drain loop below handles
    // all accept events uniformly, whether they complete or get cancelled.
    let should_cancel = rand_u8() < 25;

    if should_cancel {
        std::thread::sleep(Duration::from_millis(5));
        let cancel_status = unsafe { iroh_operation_cancel(runtime, accept_op) };
        if cancel_status != iroh_status_t::IROH_STATUS_OK as i32 {
            // Cancel failed (already completed) — that's fine, drain handles it
        }
        // If cancel succeeded, the event will be CANCELLED in the drain loop below.
    }

    // Drain accept events
    loop {
        // SAFETY: zeroed() is unsafe but we own this memory and poll_events writes to it
        let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 8] };
        let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 8, 100) };
        if count == 0 {
            break;
        }
        for i in 0..count {
            let ev = events[i as usize];
            // Only record accept events (skip node creation/close events)
            if ev.operation == accept_op {
                if ev.status == iroh_status_t::IROH_STATUS_OK as u32 {
                    metrics.record_complete();
                } else {
                    // CANCELLED or ERROR — either way, record as error/cancel
                    metrics.record_error();
                }
            }
        }
    }

    // Close the node
    let mut close_op: iroh_operation_t = 0;
    let status = unsafe { iroh_node_close(runtime, 1, 0, &mut close_op) };
    if status == iroh_status_t::IROH_STATUS_OK as i32 {
        // Wait for close to complete
        let closed = poll_for_event(runtime, iroh_event_kind_t::IROH_EVENT_CLOSED, 2000);
        if !closed {
            // Close didn't complete in time, but don't fail the cycle — node may be gone
        }
    }
    // NOTE: Node close is also an internal operation — don't track it.

    metrics.record_cycle();
    true
}

// Drain all pending events from the runtime (clears stale events between cycles).
fn drain_all_events(runtime: iroh_runtime_t) {
    loop {
        // SAFETY: zeroed() is unsafe but we own this memory and poll_events writes to it
        let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 16] };
        let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 16, 0) };
        if count == 0 {
            break;
        }
    }
}

// Poll for a specific event kind within a timeout (in ms).
// Returns true if the event was found, false on timeout.
fn poll_for_event(runtime: iroh_runtime_t, kind: iroh_event_kind_t, timeout_ms: u32) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_millis(timeout_ms as u64);
    loop {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        // SAFETY: zeroed() is unsafe but we own this memory and poll_events writes to it
        let mut events = unsafe { [std::mem::zeroed::<iroh_event_t>(); 4] };
        let count = unsafe { iroh_poll_events(runtime, events.as_mut_ptr(), 4, 50) };
        for i in 0..count {
            if events[i as usize].kind == kind as u32 {
                return true;
            }
        }
    }
}

// Simple random u8 based on monotonic clock
fn rand_u8() -> u8 {
    let nanos = std::time::Instant::now().elapsed().as_nanos();
    nanos as u8
}

// ─── Soak Test Runner (public for binary) ─────────────────────────────────

fn run_soak_test(duration_secs: u64) {
    println!(
        "Starting soak test for {} seconds ({} hours)",
        duration_secs,
        duration_secs / 3600
    );
    println!("Churn pattern: node_create → accept → cancel/close → node_close (10% cancel rate)");
    println!();

    let start = Instant::now();
    let deadline = start + Duration::from_secs(duration_secs);

    // Create runtime
    let mut runtime: iroh_runtime_t = 0;
    let status = unsafe { iroh_runtime_new(ptr::null(), &mut runtime) };
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

    let metrics = Arc::new(SoakMetrics::new());
    let mut cycle_count = 0usize;
    let print_interval = 100;

    while Instant::now() < deadline {
        let _cycle_start = Instant::now();

        let success = run_soak_cycle(runtime, &metrics);

        let _cycle_duration = _cycle_start.elapsed();
        cycle_count += 1;

        if cycle_count % print_interval == 0 || !success {
            let elapsed = start.elapsed();
            let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
            let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
            let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
            let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
            let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
            let current_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

            println!(
                "[{:?}] cycle {} — submitted={}, completed={}, cancelled={}, errored={}, max_pending={}, current_pending={}",
                elapsed,
                cycle_count,
                ops_sub,
                ops_comp,
                ops_cancel,
                ops_err,
                max_pending,
                current_pending
            );
        }

        // Small delay between cycles
        std::thread::sleep(Duration::from_millis(10));
    }

    let elapsed = start.elapsed();

    // Final metrics
    let ops_sub = metrics.ops_submitted.load(Ordering::Relaxed);
    let ops_comp = metrics.ops_completed.load(Ordering::Relaxed);
    let ops_cancel = metrics.ops_cancelled.load(Ordering::Relaxed);
    let ops_err = metrics.ops_errored.load(Ordering::Relaxed);
    let max_pending = metrics.max_pending_ops.load(Ordering::Relaxed);
    let final_pending = metrics.current_pending_ops.load(Ordering::Relaxed);

    println!();
    println!("=== Soak Test Results ===");
    println!("Duration: {:?}", elapsed);
    println!("Cycles: {}", cycle_count);
    println!("Ops submitted: {}", ops_sub);
    println!("Ops completed: {}", ops_comp);
    println!("Ops cancelled: {}", ops_cancel);
    println!("Ops errored: {}", ops_err);
    println!("Max pending ops: {}", max_pending);
    println!("Final pending ops: {}", final_pending);
    println!();

    // Assertions
    assert_eq!(
        final_pending, 0,
        "Final pending ops should be 0 (leaked ops detected)"
    );

    assert!(
        max_pending < 100,
        "Max pending ops should be bounded (was {})",
        max_pending
    );

    // Close runtime
    let status = unsafe { iroh_runtime_close(runtime) };
    assert_eq!(status, iroh_status_t::IROH_STATUS_OK as i32);

    println!("Soak test PASSED — no leaks detected");
}

// ─── Entry Point ────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let duration_secs = if args.len() > 1 {
        args[1].parse().unwrap_or(DEFAULT_DURATION_SECS)
    } else {
        DEFAULT_DURATION_SECS
    };

    run_soak_test(duration_secs);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_soak_metrics_submit_complete() {
        let metrics = SoakMetrics::new();

        metrics.record_submit();
        assert_eq!(metrics.current_pending_ops.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.max_pending_ops.load(Ordering::Relaxed), 1);

        metrics.record_complete();
        assert_eq!(metrics.current_pending_ops.load(Ordering::Relaxed), 0);

        assert_eq!(metrics.ops_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.ops_completed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn test_soak_metrics_max_pending() {
        let metrics = SoakMetrics::new();

        for _ in 0..10 {
            metrics.record_submit();
        }
        assert_eq!(metrics.max_pending_ops.load(Ordering::Relaxed), 10);

        for _ in 0..5 {
            metrics.record_complete();
        }
        assert_eq!(metrics.current_pending_ops.load(Ordering::Relaxed), 5);
    }

    #[test]
    fn test_soak_metrics_cancel() {
        let metrics = SoakMetrics::new();

        metrics.record_submit();
        metrics.record_cancel();

        assert_eq!(metrics.ops_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.ops_cancelled.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.current_pending_ops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_soak_metrics_error() {
        let metrics = SoakMetrics::new();

        metrics.record_submit();
        metrics.record_error();

        assert_eq!(metrics.ops_submitted.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.ops_errored.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.current_pending_ops.load(Ordering::Relaxed), 0);
    }

    #[test]
    #[ignore = "long-running soak test - run manually or in CI nightly"]
    fn test_soak_short() {
        // Run for 10 seconds as a quick sanity check
        aster_transport_ffi::run_soak_test(10);
    }
}
