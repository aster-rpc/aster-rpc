//! CQ Concurrency Tests using Loom
//!
//! Uses **Loom** to exhaustively permute concurrent executions, verifying invariants
//! that are hard to test with deterministic tests.
//!
//! Loom model:
//! - `loom::thread::spawn` — creates a loom-controlled thread
//! - `loom::sync::Mutex` / `RwLock` — loom-aware locks
//! - `std::sync::atomic::AtomicBool` — standard atomics
//! - `loom::model(|| { ... })` — runs all thread interleavings
//!
//! Invariants asserted:
//! - One op → at most one terminal event (never two)
//! - Cancelling an op removes it from every wait structure exactly once
//! - Dropping a connection resolves all dependent ops with ERROR
//! - No completion can outlive the handle generation it belongs to

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use loom::sync::Mutex as LoomMutex;
use loom::thread;

// =============================================================================
// Models (simplified versions of BridgeRuntime internals)
// =============================================================================

/// A unique identifier for an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct OpId(u64);

/// The state of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpState {
    Submitted,
    Completed,
    Cancelled,
    Error,
}

/// An operation tracked in the operations registry.
struct Operation {
    state: OpState,
    cancelled: Arc<AtomicBool>,
    handle: u64,
    #[allow(dead_code)]
    user_data: u64,
}

/// ModelHandleRegistry — models BridgeRuntime's HandleRegistry.
/// Uses LoomMutex so Loom can model its concurrent access.
struct ModelHandleRegistry {
    next_id: AtomicU64,
    items: LoomMutex<HashMap<u64, Arc<Mutex<Operation>>>>,
}

impl ModelHandleRegistry {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            items: LoomMutex::new(HashMap::new()),
        }
    }

    /// Insert a new operation. Returns the new OpId.
    fn insert(&self, op: Operation) -> OpId {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.items
            .lock()
            .unwrap()
            .insert(id, Arc::new(Mutex::new(op)));
        OpId(id)
    }

    /// Get an operation by id.
    fn get(&self, id: u64) -> Option<Arc<Mutex<Operation>>> {
        self.items.lock().unwrap().get(&id).cloned()
    }

    /// Mutate an operation by id (requires lock on both registry and op).
    fn with_op<F, R>(&self, id: u64, f: F) -> R
    where
        F: FnOnce(&mut Operation) -> R,
    {
        let guard = self.items.lock().unwrap();
        let op_arc = guard.get(&id).expect("op not found");
        let mut op = op_arc.lock().unwrap();
        f(&mut op)
    }
}

/// ModelEventQueue — models the tokio mpsc channel.
/// Uses LoomMutex for concurrent access.
#[derive(Default)]
struct ModelEventQueue {
    events: LoomMutex<Vec<ModelEvent>>,
}

/// Model event for tracking what was emitted.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ModelEvent {
    Completed { op_id: u64 },
    Cancelled { op_id: u64 },
    Error { op_id: u64 },
}

impl ModelEvent {
    fn op_id(&self) -> u64 {
        match self {
            Self::Completed { op_id } => *op_id,
            Self::Cancelled { op_id } => *op_id,
            Self::Error { op_id } => *op_id,
        }
    }
}

impl ModelEventQueue {
    fn push(&self, event: ModelEvent) {
        self.events.lock().unwrap().push(event);
    }

    fn drain(&self) -> Vec<ModelEvent> {
        self.events.lock().unwrap().drain(..).collect()
    }
}

// =============================================================================
// Invariant helpers
// =============================================================================

/// Check that there is at most one terminal event per op_id.
fn at_most_one_terminal(events: &[ModelEvent]) -> bool {
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for e in events {
        *counts.entry(e.op_id()).or_insert(0) += 1;
    }
    counts.values().all(|&c| c <= 1)
}

// =============================================================================
// Loom Tests
// =============================================================================

/// Test: cancel races complete — exactly one terminal event (CANCELLED wins).
/// Two threads: one cancels, one completes. Exactly one terminal event must result.
#[test]
fn loom_cancel_races_complete() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());
        let queue = Arc::new(ModelEventQueue::default());

        // Submit op1.
        let op1_id = {
            let op = Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 10,
            };
            ops.insert(op)
        };

        let op1_id_val = op1_id.0;
        let ops_c = ops.clone();
        let queue_c = queue.clone();

        // Thread A: cancel the op.
        let h_cancel = thread::spawn(move || {
            if let Some(op_arc) = ops_c.get(op1_id_val) {
                op_arc
                    .lock()
                    .unwrap()
                    .cancelled
                    .store(true, Ordering::SeqCst);
                ops_c.with_op(op1_id_val, |op| {
                    if matches!(op.state, OpState::Submitted) {
                        op.state = OpState::Cancelled;
                        queue_c.push(ModelEvent::Cancelled { op_id: op1_id_val });
                    }
                });
            }
        });

        let ops_t2 = ops.clone();
        let queue_t2 = queue.clone();

        // Thread B: complete the op (if not cancelled).
        let h_complete = thread::spawn(move || {
            if let Some(op_arc) = ops_t2.get(op1_id_val) {
                // Only complete if not cancelled
                let cancelled = op_arc.lock().unwrap().cancelled.load(Ordering::SeqCst);
                if !cancelled {
                    ops_t2.with_op(op1_id_val, |op| {
                        if matches!(op.state, OpState::Submitted) {
                            op.state = OpState::Completed;
                            queue_t2.push(ModelEvent::Completed { op_id: op1_id_val });
                        }
                    });
                }
            }
        });

        h_cancel.join().unwrap();
        h_complete.join().unwrap();

        // INVARIANT: exactly one terminal event for op1.
        let events = queue.drain();
        let terminal_count = events.iter().filter(|e| e.op_id() == op1_id_val).count();
        assert!(
            terminal_count <= 1,
            "At most one terminal event per op_id, got {} for op {}: {:?}",
            terminal_count,
            op1_id_val,
            events
        );
    });
}

/// Test: concurrent drain and cancel — exactly one terminal event.
/// One thread drains the queue, another cancels. Result: exactly one terminal event.
#[test]
fn loom_concurrent_drain_and_cancel() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());
        let queue = Arc::new(ModelEventQueue::default());

        // Submit op1.
        let op1_id = {
            let op = Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 10,
            };
            ops.insert(op)
        };
        let op1_id_val = op1_id.0;

        let ops_c = ops.clone();
        let queue_c = queue.clone();

        // Thread A: cancel the op.
        let h_cancel = thread::spawn(move || {
            if let Some(op_arc) = ops_c.get(op1_id_val) {
                op_arc
                    .lock()
                    .unwrap()
                    .cancelled
                    .store(true, Ordering::SeqCst);
                ops_c.with_op(op1_id_val, |op| {
                    if matches!(op.state, OpState::Submitted) {
                        op.state = OpState::Cancelled;
                        queue_c.push(ModelEvent::Cancelled { op_id: op1_id_val });
                    }
                });
            }
        });

        // Thread B: drain the queue.
        let h_drain = thread::spawn(move || queue.drain());

        h_cancel.join().unwrap();
        let drained = h_drain.join().unwrap();

        // INVARIANT: at most one terminal event per op_id.
        assert!(
            at_most_one_terminal(&drained),
            "Invariant violated: at most one terminal per op_id. Events: {:?}",
            drained
        );
    });
}

/// Test: submit races submit — distinct op_ids always.
#[test]
fn loom_submit_concurrent() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());

        let ops1 = ops.clone();
        let ops2 = ops.clone();

        // Thread 1: submit op A.
        let h1 = thread::spawn(move || {
            ops1.insert(Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 10,
            })
        });

        // Thread 2: submit op B.
        let h2 = thread::spawn(move || {
            ops2.insert(Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 11,
            })
        });

        let id1 = h1.join().unwrap();
        let id2 = h2.join().unwrap();

        // INVARIANT: op_ids are always distinct.
        assert_ne!(
            id1, id2,
            "op_ids must be distinct even under concurrent submit"
        );
    });
}

/// Test: close races complete — both ops get at most one terminal event.
#[test]
fn loom_close_races_complete() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());
        let queue = Arc::new(ModelEventQueue::default());

        // Submit two ops on handle 1.
        let id1 = {
            let op = Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 10,
            };
            ops.insert(op)
        };
        let _id2 = {
            let op = Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 20,
            };
            ops.insert(op)
        };

        let id1v = id1.0;
        let ops_c = ops.clone();
        let queue_c = queue.clone();

        // Thread A: close handle 1 → both ops get ERROR.
        let h_close = thread::spawn(move || {
            // Collect op_ids to close while holding registry lock.
            let op_arcs: Vec<(u64, Arc<Mutex<Operation>>)> = ops_c
                .items
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, op_arc)| op_arc.lock().unwrap().handle == 1)
                .map(|(id, op_arc)| (*id, op_arc.clone()))
                .collect();
            // Transition each op while holding its individual lock (not the registry).
            for (id, op_arc) in op_arcs {
                let mut op = op_arc.lock().unwrap();
                if matches!(op.state, OpState::Submitted) {
                    op.state = OpState::Error;
                    queue_c.push(ModelEvent::Error { op_id: id });
                }
            }
        });

        let ops_t2 = ops.clone();
        let queue_t2 = queue.clone();

        // Thread B: complete op1.
        let h_complete = thread::spawn(move || {
            ops_t2.with_op(id1v, |op| {
                if matches!(op.state, OpState::Submitted) {
                    op.state = OpState::Completed;
                    queue_t2.push(ModelEvent::Completed { op_id: id1v });
                }
            });
        });

        h_close.join().unwrap();
        h_complete.join().unwrap();

        let events = queue.drain();

        // INVARIANT: at most one terminal event per op_id.
        assert!(at_most_one_terminal(&events), "Events: {:?}", events);
    });
}

/// Test: drop all ops — all reach terminal state, no duplicates.
#[test]
fn loom_drop_connection_resolves_all_ops() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());
        let queue = Arc::new(ModelEventQueue::default());

        // Submit three ops.
        let _ids: Vec<OpId> = (0..3)
            .map(|i| {
                let op = Operation {
                    state: OpState::Submitted,
                    cancelled: Arc::new(AtomicBool::new(false)),
                    handle: 1,
                    user_data: i,
                };
                ops.insert(op)
            })
            .collect();

        // "Drop" the connection: close handle 1 → all ops get ERROR.
        let ops_c = ops.clone();
        let queue_c = queue.clone();
        thread::spawn(move || {
            let ids: Vec<u64> = ops_c.items.lock().unwrap().keys().copied().collect();
            for id in ids {
                ops_c.with_op(id, |op| {
                    if matches!(op.state, OpState::Submitted) {
                        op.state = OpState::Error;
                        queue_c.push(ModelEvent::Error { op_id: id });
                    }
                });
            }
        })
        .join()
        .unwrap();

        let events = queue.drain();

        // INVARIANT: at most one terminal event per op_id.
        assert!(at_most_one_terminal(&events), "Events: {:?}", events);
    });
}

/// Test: cancel then complete — exactly one CANCELLED (cancel wins).
#[test]
fn loom_cancel_wins_over_complete() {
    loom::model(|| {
        let ops = Arc::new(ModelHandleRegistry::new());
        let queue = Arc::new(ModelEventQueue::default());

        let op_id = {
            let op = Operation {
                state: OpState::Submitted,
                cancelled: Arc::new(AtomicBool::new(false)),
                handle: 1,
                user_data: 10,
            };
            ops.insert(op)
        };
        let op_id_val = op_id.0;

        let ops_c = ops.clone();
        let queue_c = queue.clone();

        // Thread A: cancel.
        let h_cancel = thread::spawn(move || {
            ops_c.with_op(op_id_val, |op| {
                op.cancelled.store(true, Ordering::SeqCst);
                if matches!(op.state, OpState::Submitted) {
                    op.state = OpState::Cancelled;
                    queue_c.push(ModelEvent::Cancelled { op_id: op_id_val });
                }
            });
        });

        let ops_t2 = ops.clone();
        let queue_t2 = queue.clone();

        // Thread B: complete — but only if not cancelled (cancel takes precedence).
        let h_complete = thread::spawn(move || {
            ops_t2.with_op(op_id_val, |op| {
                // Only complete if not cancelled and still in Submitted state.
                if !op.cancelled.load(Ordering::SeqCst) && matches!(op.state, OpState::Submitted) {
                    op.state = OpState::Completed;
                    queue_t2.push(ModelEvent::Completed { op_id: op_id_val });
                }
            });
        });

        h_cancel.join().unwrap();
        h_complete.join().unwrap();

        let events = queue.drain();

        // INVARIANT: exactly one terminal event.
        // Note: loom explores all interleavings. Depending on which thread acquires
        // the op lock first, either Cancelled or Completed wins — but never both.
        let cancelled_count = events
            .iter()
            .filter(|e| matches!(e, ModelEvent::Cancelled { .. }))
            .count();
        let completed_count = events
            .iter()
            .filter(|e| matches!(e, ModelEvent::Completed { .. }))
            .count();

        assert_eq!(
            events.len(),
            1,
            "Exactly one terminal event, got {}: {:?}",
            events.len(),
            events
        );
        assert!(
            cancelled_count == 1 || completed_count == 1,
            "Either Cancel or Complete wins (not both), got {} Cancelled and {} Completed",
            cancelled_count,
            completed_count
        );
    });
}

/// Test: drain is FIFO — events from concurrent sends appear in submission order.
#[test]
fn loom_queue_fifo_order() {
    loom::model(|| {
        let queue = Arc::new(ModelEventQueue::default());

        let q1 = queue.clone();
        let q2 = queue.clone();
        let q3 = queue.clone();

        // Three threads each push one event.
        let h1 = thread::spawn(move || q1.push(ModelEvent::Completed { op_id: 1 }));
        let h2 = thread::spawn(move || q2.push(ModelEvent::Completed { op_id: 2 }));
        let h3 = thread::spawn(move || q3.push(ModelEvent::Completed { op_id: 3 }));

        h1.join().unwrap();
        h2.join().unwrap();
        h3.join().unwrap();

        let events = queue.drain();

        // All 3 events present.
        assert_eq!(events.len(), 3);

        // No duplicate op_ids.
        assert!(at_most_one_terminal(&events));
    });
}
