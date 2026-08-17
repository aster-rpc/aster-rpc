//! ABI 2.0: a cancelled call must not leak per-call FFI state.
//!
//! The maps that hold a call's response sender, request receiver and
//! cancellation flag used to be cleared **only** on a successful terminal
//! submit. A cancelled handler never gets to send its trailer — its submit
//! returns `NOT_FOUND` because the channel is closed — so its three entries
//! were retained for the life of the process. Repeated cancellation grew them
//! without bound, which is remotely driven: a peer can cancel as often as it
//! likes.
//!
//! Reaping is now lifecycle-driven: `install_call_state` inserts all three
//! entries and then arms a watcher on `Sender::closed()`, which fires when core
//! drops the receiver. Both drain paths go through that one helper.
//!
//! ## SCOPE — read before trusting these
//!
//! These exercise the *mechanism* (`Sender::closed()` triggering a reap) against
//! a local `CallMaps` model. They do **not** observe the production
//! `ReactorState` maps, and that limitation has already hidden real defects: an
//! earlier version of this file passed while the watcher was armed before the
//! entries were inserted, and while the blocking-poll drain path installed no
//! watcher at all.
//!
//! Production coverage now lives in `reactor_cancel_lifecycle`, which runs a
//! real reactor over QUIC and observes `ReactorState` directly for:
//!
//!   * a call already cancelled before it is polled;
//!   * a call arriving during a *blocking* poll;
//!   * both with no subsequent submit.
//!
//! Keep these smaller tests as mechanism-level coverage, but do not treat them
//! as a substitute for that integration test again.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use aster_transport_core::reactor::{OutgoingFrame, RequestFrame, RESPONSE_CHANNEL_DEPTH};
use tokio::sync::mpsc;

/// Stand-in for one call's FFI-side state: the three maps keyed by `call_id`.
///
/// The real maps live in `REACTORS`, which needs a running reactor and a real
/// peer to populate. What is under test is the *rule* — "a closed channel reaps
/// the same entries a successful trailer does" — so this models the three
/// entries and the two paths that remove them.
#[derive(Default)]
struct CallMaps {
    senders: std::collections::HashMap<u64, mpsc::Sender<OutgoingFrame>>,
    receivers: std::collections::HashMap<u64, mpsc::Receiver<RequestFrame>>,
    flags: std::collections::HashMap<u64, Arc<AtomicBool>>,
}

impl CallMaps {
    /// Stash a sender the caller already owns, so a test can hold the matching
    /// receiver and drop it to simulate a cancellation.
    fn insert_with(&mut self, id: u64, tx: mpsc::Sender<OutgoingFrame>) {
        let (_req_tx, req_rx) = mpsc::channel(RESPONSE_CHANNEL_DEPTH);
        self.senders.insert(id, tx);
        self.receivers.insert(id, req_rx);
        self.flags.insert(id, Arc::new(AtomicBool::new(false)));
    }

    fn insert(&mut self, id: u64) -> mpsc::Receiver<OutgoingFrame> {
        let (tx, rx) = mpsc::channel(RESPONSE_CHANNEL_DEPTH);
        let (_req_tx, req_rx) = mpsc::channel(RESPONSE_CHANNEL_DEPTH);
        self.senders.insert(id, tx);
        self.receivers.insert(id, req_rx);
        self.flags.insert(id, Arc::new(AtomicBool::new(false)));
        rx
    }

    /// Mirrors `reap_call` in `ffi/src/reactor.rs`.
    fn reap(&mut self, id: u64) {
        self.senders.remove(&id);
        self.receivers.remove(&id);
        self.flags.remove(&id);
    }

    fn len(&self) -> (usize, usize, usize) {
        (self.senders.len(), self.receivers.len(), self.flags.len())
    }

    /// Mirrors a submit: reserve, and reap on a closed channel.
    fn try_submit(&mut self, id: u64) -> bool {
        // Clone out of the map so the borrow ends before `reap` needs `&mut`.
        let Some(sender) = self.senders.get(&id).cloned() else {
            return false;
        };
        // Resolve the reservation to a plain result first: the `Permit` borrows
        // `sender`, and `reap` needs `&mut self`.
        let closed = match sender.try_reserve() {
            Ok(permit) => {
                permit.send(OutgoingFrame::Frame(vec![1]));
                false
            }
            Err(mpsc::error::TrySendError::Closed(())) => true,
            Err(_) => false,
        };
        if closed {
            // The path that used to return NOT_FOUND and leak.
            self.reap(id);
            return false;
        }
        true
    }
}

/// The mechanism that actually reaps: `Sender::closed()`.
///
/// This is what the reactor spawns per call. It matters that the test does
/// **not** submit after cancelling -- the previous version did, which meant it
/// passed even though a handler that observed the cancel and simply returned
/// (the well-behaved thing to do) leaked all three entries.
#[tokio::test]
async fn cancellation_reaps_without_any_further_submit() {
    let maps = Arc::new(std::sync::Mutex::new(CallMaps::default()));
    let (tx, rx) = mpsc::channel::<OutgoingFrame>(RESPONSE_CHANNEL_DEPTH);
    maps.lock().unwrap().insert_with(1, tx.clone());
    assert_eq!(maps.lock().unwrap().len(), (1, 1, 1));

    // The watcher the FFI spawns when it stashes the sender.
    let watch_maps = maps.clone();
    let watcher = tokio::spawn(async move {
        tx.closed().await;
        watch_maps.lock().unwrap().reap(1);
    });

    // Cancellation: core drops the response receiver. Nothing else happens --
    // no submit, no binding call, no polling of the cancelled flag.
    drop(rx);

    watcher.await.expect("watcher");
    assert_eq!(
        maps.lock().unwrap().len(),
        (0, 0, 0),
        "a cancelled call must reap itself; a handler that returns without \
         submitting is correct behaviour, not a leak"
    );
}

/// Repeated cancellation, still with no submits, must not grow state.
#[tokio::test]
async fn repeated_cancellation_without_submits_keeps_the_maps_flat() {
    let maps = Arc::new(std::sync::Mutex::new(CallMaps::default()));

    for id in 0..500u64 {
        let (tx, rx) = mpsc::channel::<OutgoingFrame>(RESPONSE_CHANNEL_DEPTH);
        maps.lock().unwrap().insert_with(id, tx.clone());
        let watch_maps = maps.clone();
        let watcher = tokio::spawn(async move {
            tx.closed().await;
            watch_maps.lock().unwrap().reap(id);
        });
        drop(rx);
        watcher.await.expect("watcher");
        assert_eq!(
            maps.lock().unwrap().len(),
            (0, 0, 0),
            "state grew after cancelling call {id} with no submit"
        );
    }
}

#[tokio::test]
async fn a_cancelled_call_reaps_its_state_on_the_next_submit() {
    let mut maps = CallMaps::default();
    let rx = maps.insert(1);
    assert_eq!(maps.len(), (1, 1, 1));

    // Cancellation drops the response receiver, exactly as the reactor now does.
    drop(rx);

    // The handler's next submit sees a closed channel.
    assert!(!maps.try_submit(1), "a closed channel must not accept");
    assert_eq!(
        maps.len(),
        (0, 0, 0),
        "a cancelled call must not retain its sender, receiver or flag"
    );
}

/// The property that matters: repeated cancellation does not grow state.
#[tokio::test]
async fn repeated_cancellation_keeps_the_maps_flat() {
    let mut maps = CallMaps::default();

    for id in 0..1_000u64 {
        let rx = maps.insert(id);
        drop(rx); // cancelled
        maps.try_submit(id);
        assert_eq!(
            maps.len(),
            (0, 0, 0),
            "state grew after cancelling call {id}"
        );
    }
}

/// Reaping twice is harmless — a retry racing a cancellation can reach it more
/// than once, and it must not panic or disturb another call.
#[tokio::test]
async fn reaping_is_idempotent_and_scoped_to_one_call() {
    let mut maps = CallMaps::default();
    let rx = maps.insert(7);
    let _other = maps.insert(8);
    drop(rx);

    maps.try_submit(7);
    maps.try_submit(7);
    maps.reap(7);

    assert_eq!(
        maps.len(),
        (1, 1, 1),
        "reaping call 7 must leave call 8 alone"
    );
}
