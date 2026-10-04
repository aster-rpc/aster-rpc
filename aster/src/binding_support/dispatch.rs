//! Foreign service dispatch for bindings (design
//! `docs/_internal/bindings/dynamic-dispatch-design.md` §2.2–2.4, §4.1).
//!
//! A binding registers [`DynamicService`](crate::rpc::DynamicService)s and a
//! [`DispatchSink`] on [`AsterServerBuilder`](crate::rpc::AsterServerBuilder)
//! (`dynamic_service`, `dispatch_sink`). Rust runs every gate and delivers a
//! [`DispatchEvent::Call`] per call; the binding answers through the
//! [`CallTable`] (`AsterServer::call_table`) or the call's [`CallHandle`],
//! passing payloads and [`RpcStatus`](crate::rpc::RpcStatus)es, never frames.
//!
//! [`RingSink`] is the sink the PyO3 and C ABI bindings use: an SPSC ring
//! filled by one pump task (the former `ffi::reactor::pump_task`), drained in
//! batches by a [`RingConsumer`] that can wait asynchronously
//! ([`ready`](RingConsumer::ready)) or on a thread
//! ([`wait_blocking`](RingConsumer::wait_blocking)).

use std::sync::atomic::{fence, AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, Notify};

pub use crate::rpc::foreign::{
    CallHandle, CallRef, CallTable, CancelReason, DispatchEvent, DispatchSink, ForeignCall,
    PeerAttributes, SubmitError, FLAG_FIRST_IS_END, FLAG_IROH_AUTHENTICATED, FLAG_NEW_SESSION,
};

use super::ring;

/// A [`DispatchSink`] backed by an SPSC ring. `deliver` never blocks: it
/// enqueues on an unbounded channel whose single pump task is the ring's
/// only producer, yielding while the ring is full.
pub struct RingSink {
    tx: mpsc::UnboundedSender<DispatchEvent>,
}

/// The consuming half of a [`RingSink`]. Single consumer.
pub struct RingConsumer {
    consumer: ring::Consumer<DispatchEvent>,
    wake: Arc<Wake>,
}

/// How the pump wakes a parked consumer: a `Notify` for async waiters, and a
/// condvar guarded by a "parked" flag for threads (so the pump takes the
/// mutex only when someone is actually asleep).
struct Wake {
    notify: Notify,
    parked: AtomicBool,
    lock: Mutex<()>,
    cvar: Condvar,
    /// Set when the pump exits (every `RingSink` dropped, or the runtime
    /// shut down). Events already in the ring can still be drained.
    closed: AtomicBool,
}

impl Wake {
    fn signal(&self) {
        self.notify.notify_one();
        // Pairs with the fence in `wait_blocking`: either the consumer sees
        // the pushed item, or the pump sees `parked`.
        fence(Ordering::SeqCst);
        if self.parked.load(Ordering::Relaxed) {
            let _guard = self.lock.lock().unwrap();
            self.cvar.notify_all();
        }
    }
}

impl RingSink {
    /// A ring of `capacity` events (rounded up to a power of two) and its
    /// pump, spawned on the current tokio runtime.
    ///
    /// # Panics
    /// Outside a tokio runtime, or with `capacity == 0`.
    pub fn new(capacity: usize) -> (Arc<RingSink>, RingConsumer) {
        Self::new_on(&tokio::runtime::Handle::current(), capacity)
    }

    /// As [`new`](RingSink::new), spawning the pump on `runtime`.
    pub fn new_on(
        runtime: &tokio::runtime::Handle,
        capacity: usize,
    ) -> (Arc<RingSink>, RingConsumer) {
        let (producer, consumer) = ring::spsc(capacity);
        let (tx, rx) = mpsc::unbounded_channel();
        let wake = Arc::new(Wake {
            notify: Notify::new(),
            parked: AtomicBool::new(false),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        runtime.spawn(pump(rx, producer, wake.clone()));
        (Arc::new(RingSink { tx }), RingConsumer { consumer, wake })
    }
}

impl DispatchSink for RingSink {
    fn deliver(&self, event: DispatchEvent) {
        // A closed pump drops the event, which abandons a call in it.
        let _ = self.tx.send(event);
    }
}

/// The ring's single producer. Moves every queued event into the ring,
/// yielding while it is full, and wakes the consumer once per batch.
async fn pump(
    mut rx: mpsc::UnboundedReceiver<DispatchEvent>,
    mut producer: ring::Producer<DispatchEvent>,
    wake: Arc<Wake>,
) {
    struct Closing(Arc<Wake>);
    impl Drop for Closing {
        fn drop(&mut self) {
            self.0.closed.store(true, Ordering::Release);
            self.0.signal();
        }
    }
    let _closing = Closing(wake.clone());

    while let Some(first) = rx.recv().await {
        let mut next = Some(first);
        while let Some(mut event) = next.take() {
            loop {
                match producer.try_push(event) {
                    Ok(()) => break,
                    Err(back) => {
                        event = back;
                        // Full: let the consumer catch up. Wake it first in
                        // case it parked before this batch's signal.
                        wake.signal();
                        tokio::task::yield_now().await;
                    }
                }
            }
            next = rx.try_recv().ok();
        }
        wake.signal();
    }
}

impl RingConsumer {
    /// Take up to `max` events, calling `f` for each. Returns how many.
    pub fn drain(&mut self, max: usize, f: impl FnMut(DispatchEvent)) -> usize {
        self.consumer.drain(max, f)
    }

    /// Events ready to drain.
    pub fn len(&self) -> usize {
        self.consumer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the pump has stopped. Remaining events can still be drained.
    pub fn is_closed(&self) -> bool {
        self.wake.closed.load(Ordering::Acquire)
    }

    /// Resolve once there is something to drain, or the pump stopped.
    pub async fn ready(&self) {
        loop {
            let notified = self.wake.notify.notified();
            if !self.is_empty() || self.is_closed() {
                return;
            }
            notified.await;
        }
    }

    /// Block the calling thread until there is something to drain, the pump
    /// stopped, or `timeout` passes. Returns whether events are available.
    /// For poll threads (the C ABI); never call it on a runtime worker.
    pub fn wait_blocking(&self, timeout: Duration) -> bool {
        if !self.is_empty() {
            return true;
        }
        let deadline = std::time::Instant::now() + timeout;
        let mut guard = self.wake.lock.lock().unwrap();
        loop {
            self.wake.parked.store(true, Ordering::Relaxed);
            fence(Ordering::SeqCst);
            if !self.is_empty() || self.is_closed() {
                break;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            guard = self
                .wake
                .cvar
                .wait_timeout(guard, deadline - now)
                .unwrap()
                .0;
        }
        self.wake.parked.store(false, Ordering::Relaxed);
        drop(guard);
        !self.is_empty()
    }
}
