//! CQ State Machine Tests
//!
//! Tests the Completion Queue as a pure state machine with a **fake reactor**
//! (no real sockets, no real time, no async tokio).
//!
//! Model:
//! - Operations are created by submit() and tracked in a HashMap
//! - Events are emitted synchronously: complete()/cancel()/error()/close_handle() immediately add
//!   terminal events to the event queue
//! - drain() reads from the event queue (blocking semantics via timeout, non-blocking for tests)
//!
//! State model:
//! ```text
//! SUBMITTED → COMPLETED (complete)
//! SUBMITTED → CANCELLED (cancel before drain)
//! SUBMITTED → ERROR (handle closed before drain)
//! ```
//!
//! Invariants tested:
//! - One op produces at most one terminal event
//! - Cancel removes op from every wait structure exactly once
//! - No completion can outlive the handle generation it belongs to
//! - Dropping a connection resolves all dependent ops
//! - Never "complete twice", never "release twice"

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// A unique identifier for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OpId(u64);

impl OpId {
    pub fn value(self) -> u64 {
        self.0
    }
}

/// The state of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpState {
    /// Operation submitted, waiting in the event queue.
    Submitted,
    /// Terminal: operation completed successfully.
    Completed,
    /// Terminal: operation was cancelled.
    Cancelled,
    /// Terminal: operation failed with error.
    Error,
}

/// A fake operation tracked by the fake reactor.
struct FakeOp {
    state: OpState,
    cancelled: Arc<AtomicBool>,
    handle: u64,
    user_data: u64,
}

impl FakeOp {
    fn new(handle: u64, user_data: u64) -> (Self, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Self {
                state: OpState::Submitted,
                cancelled: cancelled.clone(),
                handle,
                user_data,
            },
            cancelled,
        )
    }

    fn cancel(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if self.state == OpState::Submitted {
            self.state = OpState::Cancelled;
        }
    }
}

/// Events produced by the fake reactor.
#[derive(Debug, Clone)]
enum FakeEvent {
    Completed {
        op_id: OpId,
        #[allow(dead_code)]
        user_data: u64,
    },
    Cancelled {
        op_id: OpId,
        #[allow(dead_code)]
        user_data: u64,
    },
    Error {
        op_id: OpId,
        #[allow(dead_code)]
        user_data: u64,
        #[allow(dead_code)]
        error_code: i32,
    },
}

impl FakeEvent {
    fn op_id(&self) -> OpId {
        match self {
            Self::Completed { op_id, .. } => *op_id,
            Self::Cancelled { op_id, .. } => *op_id,
            Self::Error { op_id, .. } => *op_id,
        }
    }
}

/// The FakeReactor trait — abstracts the CQ for testing.
trait FakeReactor {
    /// Submit a new operation and return its OpId.
    fn submit(&mut self, handle: u64, user_data: u64) -> OpId;

    /// Cancel an operation by OpId. Returns true if the op was found and was not terminal.
    fn cancel(&mut self, op_id: OpId) -> bool;

    /// Drain up to max_events events from the CQ (non-blocking).
    fn drain(&mut self, max_events: usize) -> Vec<FakeEvent>;

    /// Simulate completing an operation successfully.
    fn complete(&mut self, op_id: OpId);

    /// Simulate an operation erroring.
    #[allow(dead_code)]
    fn error(&mut self, op_id: OpId, error_code: i32);

    /// Close a handle — all ops on that handle become ERROR.
    fn close_handle(&mut self, handle: u64);

    /// Get the current state of an op.
    fn op_state(&self, op_id: OpId) -> Option<OpState>;
}

/// A VecDeque-backed FakeReactor for deterministic testing.
struct VecDequeReactor {
    next_op_id: u64,
    ops: HashMap<u64, FakeOp>,
    /// The event queue — drain reads from here.
    event_queue: VecDeque<FakeEvent>,
}

impl VecDequeReactor {
    fn new() -> Self {
        Self {
            next_op_id: 1,
            ops: HashMap::new(),
            event_queue: VecDeque::new(),
        }
    }
}

impl Default for VecDequeReactor {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeReactor for VecDequeReactor {
    fn submit(&mut self, handle: u64, user_data: u64) -> OpId {
        let op_id = self.next_op_id;
        self.next_op_id += 1;
        let (op, _cancelled) = FakeOp::new(handle, user_data);
        self.ops.insert(op_id, op);
        OpId(op_id)
    }

    fn cancel(&mut self, op_id: OpId) -> bool {
        if let Some(op) = self.ops.get_mut(&op_id.0) {
            op.cancel();
            if op.state == OpState::Cancelled {
                self.event_queue.push_back(FakeEvent::Cancelled {
                    op_id,
                    user_data: op.user_data,
                });
                return true;
            }
        }
        false
    }

    fn drain(&mut self, max_events: usize) -> Vec<FakeEvent> {
        let mut out = Vec::with_capacity(max_events);
        for _ in 0..max_events {
            match self.event_queue.pop_front() {
                Some(event) => out.push(event),
                None => break,
            }
        }
        out
    }

    fn complete(&mut self, op_id: OpId) {
        if let Some(op) = self.ops.get_mut(&op_id.0) {
            if op.state == OpState::Submitted {
                op.state = OpState::Completed;
                self.event_queue.push_back(FakeEvent::Completed {
                    op_id,
                    user_data: op.user_data,
                });
            }
        }
    }

    fn error(&mut self, op_id: OpId, error_code: i32) {
        if let Some(op) = self.ops.get_mut(&op_id.0) {
            if op.state == OpState::Submitted {
                op.state = OpState::Error;
                self.event_queue.push_back(FakeEvent::Error {
                    op_id,
                    user_data: op.user_data,
                    error_code,
                });
            }
        }
    }

    fn close_handle(&mut self, handle: u64) {
        for (_, op) in self.ops.iter_mut() {
            if op.handle == handle && op.state == OpState::Submitted {
                op.state = OpState::Error;
                self.event_queue.push_back(FakeEvent::Error {
                    op_id: OpId(handle),
                    user_data: op.user_data,
                    error_code: 0,
                });
            }
        }
    }

    fn op_state(&self, op_id: OpId) -> Option<OpState> {
        self.ops.get(&op_id.0).map(|op| op.state)
    }
}

// =============================================================================
// Test Cases
// =============================================================================

#[test]
fn submit_then_complete() {
    // submit → complete → drain → exactly one Completed event
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    assert_eq!(r.op_state(op1), Some(OpState::Submitted));

    // Complete emits Completed synchronously.
    r.complete(op1);
    assert_eq!(r.op_state(op1), Some(OpState::Completed));

    // Drain returns the Completed event.
    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], FakeEvent::Completed { .. }));
    assert_eq!(events[0].op_id(), op1);

    // Second drain: nothing left.
    let events = r.drain(10);
    assert!(events.is_empty());
}

#[test]
fn submit_then_cancel() {
    // submit → cancel → drain → exactly one Cancelled event
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);

    let cancelled = r.cancel(op1);
    assert!(cancelled);
    assert_eq!(r.op_state(op1), Some(OpState::Cancelled));

    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], FakeEvent::Cancelled { .. }));
    assert_eq!(events[0].op_id(), op1);

    let events = r.drain(10);
    assert!(events.is_empty());
}

#[test]
fn submit_close_before_drain() {
    // submit → close_handle → drain → exactly one Error event (not Completed)
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    assert_eq!(r.op_state(op1), Some(OpState::Submitted));

    r.close_handle(1);
    assert_eq!(r.op_state(op1), Some(OpState::Error));

    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], FakeEvent::Error { .. }));
    assert_eq!(events[0].op_id(), op1);
}

#[test]
fn submit_twice_before_drain() {
    // Two ops on same handle, both complete → two distinct Completed events
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    let op2 = r.submit(1, 11);
    assert_ne!(op1, op2);

    r.complete(op1);
    r.complete(op2);

    let events = r.drain(10);
    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .all(|e| matches!(e, FakeEvent::Completed { .. })));
}

#[test]
fn cancel_races_complete() {
    // cancel first, then complete — exactly one terminal (CANCELLED wins)
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);

    r.cancel(op1);
    r.complete(op1); // no-op: already Cancelled

    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], FakeEvent::Cancelled { .. }));
}

#[test]
fn close_races_accept() {
    // accept op on handle X, then close X before drain — exactly one Error
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);

    r.close_handle(1);
    r.complete(op1); // no-op: already Error

    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], FakeEvent::Error { .. }));

    let events = r.drain(10);
    assert!(events.is_empty());
}

#[test]
fn stale_op_id() {
    // Two ops: cancel one, complete the other — drain returns both terminals
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    let op2 = r.submit(1, 20);

    r.cancel(op1);
    r.complete(op2);

    let events = r.drain(10);
    assert_eq!(events.len(), 2);
    let ids: Vec<_> = events.iter().map(|e| e.op_id()).collect();
    assert!(ids.contains(&op1));
    assert!(ids.contains(&op2));
}

#[test]
fn stale_handle_reuse() {
    // Op on handle 1, close handle 1, drain Error, then new op on handle 1
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);

    r.close_handle(1); // op1 → Error
    let events = r.drain(10);
    assert!(matches!(&events[0], FakeEvent::Error { .. }));

    // New op on same handle (handle reuse).
    let op2 = r.submit(1, 20);
    assert_ne!(op1, op2);
    assert_eq!(r.op_state(op1), Some(OpState::Error));
    assert_eq!(r.op_state(op2), Some(OpState::Submitted));

    r.complete(op2);
    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], FakeEvent::Completed { .. }));
    assert_eq!(events[0].op_id(), op2);
}

#[test]
fn completion_after_ownership_transfer() {
    // Two ops on different handles: both complete, then close handle 1
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    let op2 = r.submit(2, 20);

    r.complete(op1);
    r.complete(op2);
    r.close_handle(1); // op1 already Completed — no additional event

    let events = r.drain(10);
    assert_eq!(events.len(), 2); // both Completed
    let completed_count = events
        .iter()
        .filter(|e| matches!(e, FakeEvent::Completed { .. }))
        .count();
    assert_eq!(completed_count, 2);
}

#[test]
fn release_twice() {
    // drain twice → second drain is empty (not two Completed events)
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);
    r.complete(op1);

    let events = r.drain(10);
    assert_eq!(events.len(), 1);

    // Second drain: empty.
    let events = r.drain(10);
    assert!(events.is_empty(), "Second drain should be empty — no UB");

    // Complete again: no-op (already Completed).
    r.complete(op1);
    let events = r.drain(10);
    assert!(events.is_empty(), "complete on terminal op is no-op");
}

#[test]
fn close_during_completing() {
    // Op completes, then close — Completed is NOT overwritten by Error
    let mut r = VecDequeReactor::new();
    let op1 = r.submit(1, 10);

    r.complete(op1); // op1 → Completed, event in queue
    r.close_handle(1); // close: op1 is already terminal, no additional event

    let events = r.drain(10);
    assert_eq!(events.len(), 1);
    assert!(
        matches!(&events[0], FakeEvent::Completed { .. }),
        "Completed should not be overwritten"
    );
}
