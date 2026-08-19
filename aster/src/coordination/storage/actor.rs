//! The single-writer actor shutdown guard, shared by both databases (§11.1).
//!
//! ## Why this is one type rather than one per database
//!
//! The ordering it enforces is easy to get wrong and silent when you do: the
//! actor thread owns the `Database`, so dropping a store only *asks* it to
//! stop. If the directory lock were released before the thread actually exited,
//! the next open of that directory would find the file still held — a restart
//! failing with `DatabaseAlreadyOpen` in the best case, and in the worst a
//! second voter admitted while the first still has the file open.
//!
//! `raft.redb` and `state.redb` each get an actor, and both need exactly that
//! ordering. Written twice it would be two places to get it wrong, and the
//! failure mode does not show up in a test that closes cleanly — which is every
//! test that does not restart.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::mpsc;

/// Stops an actor and waits for it to release its database.
///
/// Dropped **before** the directory lock, which the field order in
/// [`VoterStorage`](super::VoterStorage) is what guarantees.
pub(crate) struct ActorShutdown<C: Send + 'static> {
    tx: mpsc::Sender<C>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Builds the command that asks the actor to stop.
    shutdown: fn() -> C,
}

/// Hand-written because the command types are not `Debug`: an actor command
/// carries a reply channel and, on the write paths, a whole apply batch, and
/// neither is something a store's `Debug` should print.
impl<C: Send + 'static> std::fmt::Debug for ActorShutdown<C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorShutdown")
            .field("stopping", &self.stop.load(Ordering::SeqCst))
            .field("joined", &self.thread.is_none())
            .finish()
    }
}

impl<C: Send + 'static> ActorShutdown<C> {
    pub(crate) fn new(
        tx: mpsc::Sender<C>,
        stop: Arc<AtomicBool>,
        thread: std::thread::JoinHandle<()>,
        shutdown: fn() -> C,
    ) -> Self {
        Self {
            tx,
            stop,
            thread: Some(thread),
            shutdown,
        }
    }
}

impl<C: Send + 'static> Drop for ActorShutdown<C> {
    fn drop(&mut self) {
        // The flag handles the case where the queue is full and `try_send`
        // cannot get the stop command in: the actor checks it before servicing
        // each command, so it converges either way. `try_send` is used rather
        // than a blocking send because `Drop` may run on a Tokio worker, where
        // blocking would stall the runtime.
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.tx.try_send((self.shutdown)());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
