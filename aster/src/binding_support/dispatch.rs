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

use tokio::sync::{mpsc, Notify, OwnedSemaphorePermit, Semaphore};

use crate::rpc::foreign::DispatchEventPermit;
pub use crate::rpc::foreign::{
    CallHandle, CallRef, CallTable, CallTraceContext, CancelReason, DispatchAdmission,
    DispatchEvent, DispatchPermit, DispatchSendError, DispatchSink, ForeignCall, InboundTrace,
    PeerAttributes, SubmitError, FLAG_FIRST_IS_END, FLAG_IROH_AUTHENTICATED, FLAG_NEW_SESSION,
};

use super::ring;

/// Default independent bound for live foreign calls, sessions, connections,
/// and transferred non-RPC connections.
pub const DEFAULT_MAX_FOREIGN_INFLIGHT: usize = 4096;

/// A [`DispatchSink`] backed by an SPSC ring and finite staging queue.
pub struct RingSink {
    tx: mpsc::Sender<QueuedEvent>,
    call_admissions: Arc<Semaphore>,
    session_admissions: Arc<Semaphore>,
    connection_admissions: Arc<Semaphore>,
    non_rpc_admissions: Arc<Semaphore>,
    external: Arc<Semaphore>,
    wake: Arc<Wake>,
}

struct QueuedEvent {
    event: DispatchEvent,
    handoff: DispatchHandoff,
}

/// Opaque ownership retained until an event reaches the foreign consumer.
/// Bindings that convert events in a later callback must retain this value
/// through that conversion.
pub struct DispatchHandoff {
    _event: Option<DispatchEventPermit>,
    _external: Option<OwnedSemaphorePermit>,
}

/// The consuming half of a [`RingSink`]. Single consumer.
pub struct RingConsumer {
    consumer: ring::Consumer<QueuedEvent>,
    wake: Arc<Wake>,
}

/// Cloneable pump notification for integrations whose callback scheduler is
/// separate from the ring consumer (for example a Node threadsafe function).
#[derive(Clone)]
pub struct RingNotifier {
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
    consumer_alive: AtomicBool,
    space: Notify,
    consumer_closed: Notify,
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
        Self::new_with_limit(capacity, DEFAULT_MAX_FOREIGN_INFLIGHT)
    }

    pub fn new_with_limit(capacity: usize, max_inflight: usize) -> (Arc<RingSink>, RingConsumer) {
        Self::new_on_with_limit(&tokio::runtime::Handle::current(), capacity, max_inflight)
    }

    /// As [`new`](RingSink::new), spawning the pump on `runtime`.
    pub fn new_on(
        runtime: &tokio::runtime::Handle,
        capacity: usize,
    ) -> (Arc<RingSink>, RingConsumer) {
        Self::new_on_with_limit(runtime, capacity, DEFAULT_MAX_FOREIGN_INFLIGHT)
    }

    pub fn new_on_with_limit(
        runtime: &tokio::runtime::Handle,
        capacity: usize,
        max_inflight: usize,
    ) -> (Arc<RingSink>, RingConsumer) {
        assert!(
            max_inflight > 0,
            "max inflight foreign entities must be > 0"
        );
        let (producer, consumer) = ring::spsc(capacity);
        let wake = Arc::new(Wake {
            notify: Notify::new(),
            parked: AtomicBool::new(false),
            lock: Mutex::new(()),
            cvar: Condvar::new(),
            closed: AtomicBool::new(false),
            consumer_alive: AtomicBool::new(true),
            space: Notify::new(),
            consumer_closed: Notify::new(),
        });
        // Each of four entity kinds admits at most E entities, each with at
        // most four outstanding lifecycle events. Public unadmitted
        // producers have a separate E-event quota.
        let staging_capacity = max_inflight
            .checked_mul(17)
            .expect("max inflight foreign entities is too large");
        let (tx, rx) = mpsc::channel(staging_capacity);
        runtime.spawn(pump(rx, producer, wake.clone()));
        (
            Arc::new(RingSink {
                tx,
                call_admissions: Arc::new(Semaphore::new(max_inflight)),
                session_admissions: Arc::new(Semaphore::new(max_inflight)),
                connection_admissions: Arc::new(Semaphore::new(max_inflight)),
                non_rpc_admissions: Arc::new(Semaphore::new(max_inflight)),
                external: Arc::new(Semaphore::new(max_inflight)),
                wake: wake.clone(),
            }),
            RingConsumer { consumer, wake },
        )
    }

    fn enqueue(
        &self,
        event: DispatchEvent,
        handoff: DispatchHandoff,
    ) -> Result<(), DispatchSendError<(DispatchEvent, DispatchHandoff)>> {
        if !self.wake.consumer_alive.load(Ordering::Acquire) {
            return Err(DispatchSendError::Closed((event, handoff)));
        }
        let queued = QueuedEvent { event, handoff };
        match self.tx.try_send(queued) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(queued)) => {
                Err(DispatchSendError::Closed((queued.event, queued.handoff)))
            }
            Err(mpsc::error::TrySendError::Full(queued)) => {
                Err(DispatchSendError::Full((queued.event, queued.handoff)))
            }
        }
    }
}

impl DispatchSink for RingSink {
    fn try_admit(&self, kind: DispatchAdmission) -> Option<DispatchPermit> {
        if self.tx.is_closed() || !self.wake.consumer_alive.load(Ordering::Acquire) {
            return None;
        }
        let admissions = match kind {
            DispatchAdmission::Call => &self.call_admissions,
            DispatchAdmission::Session => &self.session_admissions,
            DispatchAdmission::Connection => &self.connection_admissions,
            DispatchAdmission::NonRpc => &self.non_rpc_admissions,
        };
        admissions
            .clone()
            .try_acquire_owned()
            .ok()
            .map(DispatchPermit::new)
    }

    fn deliver(&self, event: DispatchEvent) -> Result<(), DispatchSendError<DispatchEvent>> {
        if !self.wake.consumer_alive.load(Ordering::Acquire) {
            return Err(DispatchSendError::Closed(event));
        }
        let external = match self.external.clone().try_acquire_owned() {
            Ok(external) => external,
            Err(_) if self.tx.is_closed() || !self.wake.consumer_alive.load(Ordering::Acquire) => {
                return Err(DispatchSendError::Closed(event));
            }
            Err(_) => return Err(DispatchSendError::Full(event)),
        };
        self.enqueue(
            event,
            DispatchHandoff {
                _event: None,
                _external: Some(external),
            },
        )
        .map_err(|error| match error {
            DispatchSendError::Full((event, _)) => DispatchSendError::Full(event),
            DispatchSendError::Closed((event, _)) => DispatchSendError::Closed(event),
        })
    }

    fn deliver_for(
        &self,
        event: DispatchEvent,
        permit: &DispatchPermit,
    ) -> Result<(), DispatchSendError<DispatchEvent>> {
        let Some(event_permit) = permit.event() else {
            return Err(DispatchSendError::Full(event));
        };
        self.enqueue(
            event,
            DispatchHandoff {
                _event: Some(event_permit),
                _external: None,
            },
        )
        .map_err(|error| match error {
            DispatchSendError::Full((event, _)) => DispatchSendError::Full(event),
            DispatchSendError::Closed((event, _)) => DispatchSendError::Closed(event),
        })
    }

    fn deliver_admitted(
        &self,
        event: DispatchEvent,
        permit: DispatchPermit,
    ) -> Result<(), DispatchSendError<(DispatchEvent, DispatchPermit)>> {
        let Some(event_permit) = permit.event() else {
            return Err(DispatchSendError::Full((event, permit)));
        };
        self.enqueue(
            event,
            DispatchHandoff {
                _event: Some(event_permit),
                _external: None,
            },
        )
        .map_err(|error| match error {
            DispatchSendError::Full((event, _)) => DispatchSendError::Full((event, permit)),
            DispatchSendError::Closed((event, _)) => DispatchSendError::Closed((event, permit)),
        })
    }
}

/// The ring's single producer. Moves every queued event into the ring,
/// yielding while it is full, and wakes the consumer once per batch.
async fn pump(
    mut rx: mpsc::Receiver<QueuedEvent>,
    mut producer: ring::Producer<QueuedEvent>,
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

    loop {
        let first = tokio::select! {
            event = rx.recv() => match event {
                Some(event) => event,
                None => break,
            },
            _ = wake.consumer_closed.notified() => break,
        };
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
                        let notified = wake.space.notified();
                        if !wake.consumer_alive.load(Ordering::Acquire) {
                            return;
                        }
                        notified.await;
                    }
                }
            }
            next = rx.try_recv().ok();
        }
        wake.signal();
    }
}

impl RingConsumer {
    /// A notification that fires after the pump signals newly ready work, or
    /// when the pump closes. It never observes the staging queue early.
    pub fn notifier(&self) -> RingNotifier {
        RingNotifier {
            wake: self.wake.clone(),
        }
    }

    /// Close producer admission and wake the pump. Staged and in-flight
    /// events are dropped by the pump; events already in the ring remain
    /// available to drain.
    pub fn close(&self) {
        self.wake.consumer_alive.store(false, Ordering::Release);
        self.wake.space.notify_waiters();
        self.wake.consumer_closed.notify_one();
    }
    /// Drain events while transferring their optional final-entity permit to
    /// the consumer. Callback bridges keep the permit until the event has
    /// been converted and handed to the foreign runtime.
    pub fn drain_admitted(
        &mut self,
        max: usize,
        mut f: impl FnMut(DispatchEvent, DispatchHandoff),
    ) -> usize {
        self.consumer.drain(max, |queued| {
            self.wake.space.notify_one();
            let QueuedEvent { event, handoff } = queued;
            f(event, handoff);
        })
    }

    /// Take up to `max` events, calling `f` for each. Returns how many.
    pub fn drain(&mut self, max: usize, f: impl FnMut(DispatchEvent)) -> usize {
        let mut f = f;
        self.drain_admitted(max, |event, handoff| {
            f(event);
            drop(handoff);
        })
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

impl RingNotifier {
    /// Wait for a pump signal. Returns `false` once the pump has closed.
    pub async fn ready(&self) -> bool {
        let notified = self.wake.notify.notified();
        if self.wake.closed.load(Ordering::Acquire) {
            return false;
        }
        notified.await;
        !self.wake.closed.load(Ordering::Acquire)
    }
}

impl Drop for RingConsumer {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(call_id: u64) -> DispatchEvent {
        DispatchEvent::Cancelled {
            call_id,
            reason: CancelReason::Peer,
        }
    }

    fn id(event: DispatchEvent) -> u64 {
        match event {
            DispatchEvent::Cancelled { call_id, .. } => call_id,
            _ => panic!("unexpected event"),
        }
    }

    #[tokio::test]
    async fn reentrant_handoff_has_four_ordered_entity_credits() {
        let (sink, mut consumer) = RingSink::new_with_limit(1, 1);
        let permit = sink.try_admit(DispatchAdmission::Call).unwrap();
        assert!(sink.deliver_for(event(1), &permit).is_ok());
        consumer.ready().await;

        let mut seen = Vec::new();
        consumer.drain_admitted(1, |first, _handoff| {
            seen.push(id(first));
            for call_id in 2..=4 {
                assert!(sink.deliver_for(event(call_id), &permit).is_ok());
            }
            assert!(matches!(
                sink.deliver_for(event(5), &permit),
                Err(DispatchSendError::Full(_))
            ));
        });
        while seen.len() < 4 {
            consumer.ready().await;
            consumer.drain(4, |next| seen.push(id(next)));
        }
        assert_eq!(seen, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn public_full_and_consumer_drop_return_event_ownership() {
        let (sink, consumer) = RingSink::new_with_limit(1, 1);
        let wake = consumer.wake.clone();
        assert!(sink.deliver(event(1)).is_ok());
        let returned = match sink.deliver(event(2)) {
            Err(DispatchSendError::Full(event)) => event,
            _ => panic!("second public event should be returned as full"),
        };
        assert_eq!(id(returned), 2);

        drop(consumer);
        tokio::task::yield_now().await;
        let returned = match sink.deliver(event(3)) {
            Err(DispatchSendError::Closed(event)) => event,
            _ => panic!("event should be returned after consumer drop"),
        };
        assert_eq!(id(returned), 3);
        tokio::time::timeout(Duration::from_secs(1), async {
            while !wake.closed.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("pump did not exit after its consumer was dropped");
    }

    #[tokio::test]
    async fn explicit_close_and_drain_release_final_permits() {
        let (sink, mut consumer) = RingSink::new_with_limit(1, 1);
        let permit = sink.try_admit(DispatchAdmission::Call).unwrap();
        assert!(sink.deliver_admitted(event(1), permit).is_ok());
        consumer.close();

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                consumer.drain(1, drop);
                if sink.call_admissions.available_permits() == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("staged final permit survived consumer close");
    }
}
