//! Outstanding reads on `state.redb`, and the ability to wait them out (§11.1).
//!
//! ## What this exists to stop
//!
//! A redb `ReadTransaction` owns an `Arc<TransactionalMemory>`, so it is
//! `'static`: it keeps the pinned version alive on its own and does not borrow
//! the `Database` it came from. That is exactly what makes §11.4's "streams
//! without blocking the writer" implementable — and it means a snapshot builder
//! can outlive the store that issued it.
//!
//! Two things then go wrong, and both are silent. The directory `LOCK` is
//! released when [`VoterStorage`](super::VoterStorage) drops, so a second
//! opener could be admitted while a builder is still reading the old database —
//! and could still publish `CURRENT` afterwards, from a version the new owner
//! knows nothing about. And install cannot honour §11.4's "close the old
//! handles" before replacing `state.redb`, because it has no way to find out
//! whether any are open.
//!
//! Both are the same missing fact: *how many reads are in flight*. One counter
//! answers it, and the two operations that need it are the same wait.
//!
//! ## Why a lease rather than a lifetime
//!
//! A `'a`-bound read transaction would have expressed this in the type system,
//! but redb's is not one, and wrapping it in a borrow would give the transaction
//! back the property this subsystem wanted removed — it could no longer cross to
//! a blocking thread. So the count is dynamic and the guard is a value; what
//! makes it structural is that [`ReadGate::issue`] is the only way to obtain a
//! [`GuardedRead`], and the transaction is not reachable without one.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity.
#![allow(dead_code)]

use std::sync::{Arc, Condvar, Mutex};

#[derive(Debug, Default)]
struct GateState {
    outstanding: usize,
    /// Cleared while a replacement waits, and while the store is closing.
    admitting: bool,
}

#[derive(Debug)]
struct GateInner {
    state: Mutex<GateState>,
    idle: Condvar,
}

/// Counts the read transactions handed out over `state.redb`.
#[derive(Debug, Clone)]
pub(crate) struct ReadGate {
    inner: Arc<GateInner>,
}

impl Default for ReadGate {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadGate {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(GateInner {
                state: Mutex::new(GateState {
                    outstanding: 0,
                    admitting: true,
                }),
                idle: Condvar::new(),
            }),
        }
    }

    /// Pair a read transaction with a lease, or refuse once the gate is shut.
    ///
    /// Refusing rather than queueing is deliberate: the two things that shut
    /// the gate are a store that is closing and a replacement that is about to
    /// swap the file out from under this version. Neither has a transaction
    /// worth waiting for — the caller wants a read of the *current* database,
    /// and after either of those the current database is not this one.
    pub(crate) fn issue(&self, transaction: redb::ReadTransaction) -> Option<GuardedRead> {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        if !state.admitting {
            return None;
        }
        state.outstanding += 1;
        Some(GuardedRead {
            transaction,
            _lease: ReadLease {
                inner: Arc::clone(&self.inner),
            },
        })
    }

    /// Stop admitting reads and block until every outstanding one has ended.
    fn shut(&self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.admitting = false;
        while state.outstanding > 0 {
            state = self.inner.idle.wait(state).expect("read gate mutex");
        }
    }

    /// Exclusive access to the database file: no read may be open, and none is
    /// admitted, until the returned value is dropped.
    ///
    /// This is what install needs before renaming a new database over
    /// `state.redb`. Reads resume when it drops, because a replacement is a
    /// pause rather than an end.
    pub(crate) fn quiesce(&self) -> Quiesced {
        self.shut();
        Quiesced {
            inner: Arc::clone(&self.inner),
        }
    }

    /// The shutdown counterpart: shut the gate and never reopen it.
    ///
    /// Held by [`ReadDrain`] rather than called directly, so that "the lock is
    /// not released while a read is live" is a consequence of field order in
    /// [`VoterStorage`](super::VoterStorage) — the same mechanism that already
    /// orders the actors against the lock — rather than of remembering to call
    /// something.
    fn close(&self) {
        self.shut();
    }

    #[cfg(test)]
    pub(crate) fn outstanding(&self) -> usize {
        self.inner
            .state
            .lock()
            .expect("read gate mutex")
            .outstanding
    }

    #[cfg(test)]
    pub(crate) fn is_admitting(&self) -> bool {
        self.inner.state.lock().expect("read gate mutex").admitting
    }
}

/// One outstanding read. Dropping it is what lets a waiter proceed.
#[derive(Debug)]
struct ReadLease {
    inner: Arc<GateInner>,
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.outstanding = state.outstanding.saturating_sub(1);
        if state.outstanding == 0 {
            self.inner.idle.notify_all();
        }
    }
}

/// A read transaction and the lease that keeps the store open around it.
///
/// The transaction is reachable only through this, so there is no way to hold
/// one without holding the lease.
#[derive(Debug)]
pub(crate) struct GuardedRead {
    transaction: redb::ReadTransaction,
    _lease: ReadLease,
}

impl std::ops::Deref for GuardedRead {
    type Target = redb::ReadTransaction;

    fn deref(&self) -> &redb::ReadTransaction {
        &self.transaction
    }
}

/// Exclusive access, released on drop.
#[derive(Debug)]
pub(crate) struct Quiesced {
    inner: Arc<GateInner>,
}

impl Drop for Quiesced {
    fn drop(&mut self) {
        self.inner.state.lock().expect("read gate mutex").admitting = true;
    }
}

/// Shuts the gate and waits for outstanding reads when dropped.
///
/// Its position in [`VoterStorage`](super::VoterStorage)'s field list is the
/// whole point: it is dropped before the state actor releases the `Database`
/// and long before the directory lock goes, so neither can happen while a
/// builder is still reading.
#[derive(Debug)]
pub(crate) struct ReadDrain {
    gate: ReadGate,
}

impl ReadDrain {
    pub(crate) fn new(gate: ReadGate) -> Self {
        Self { gate }
    }
}

impl Drop for ReadDrain {
    fn drop(&mut self) {
        self.gate.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A gate with nothing outstanding is not something to wait for.
    #[test]
    fn an_idle_gate_quiesces_immediately() {
        let gate = ReadGate::new();
        let held = gate.quiesce();
        assert_eq!(gate.outstanding(), 0);
        assert!(!gate.is_admitting(), "no read is admitted while it is held");
        drop(held);
        assert!(gate.is_admitting(), "and reads resume afterwards");
    }

    /// A quiesce does not return while a read is outstanding, and returns once
    /// it ends.
    ///
    /// The negative half is a timing observation and can only fail in the safe
    /// direction: if the wait were missing entirely the send would arrive at
    /// once and the `recv_timeout` below would succeed. The positive half is
    /// exact — the completion cannot arrive before the lease is dropped,
    /// because nothing else drops it.
    #[test]
    fn a_quiesce_waits_for_an_outstanding_read() {
        let gate = ReadGate::new();
        // A gate with a lease but no real transaction is not expressible, so
        // the test issues one over a scratch database — the same path the actor
        // uses.
        let (_dir, database) = scratch_database();
        let read = gate
            .issue(redb::ReadableDatabase::begin_read(&database).expect("read"))
            .expect("an open gate issues");
        assert_eq!(gate.outstanding(), 1);

        let (tx, rx) = mpsc::channel();
        let waiting = gate.clone();
        let thread = std::thread::spawn(move || {
            let held = waiting.quiesce();
            tx.send(()).expect("report");
            drop(held);
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "a quiesce must not return while a read is still open"
        );
        drop(read);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("and must return once it ends");
        thread.join().expect("join");
        assert_eq!(gate.outstanding(), 0);
    }

    /// A shut gate refuses rather than queues.
    #[test]
    fn a_shut_gate_issues_nothing() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let held = gate.quiesce();
        assert!(gate
            .issue(redb::ReadableDatabase::begin_read(&database).expect("read"))
            .is_none());
        drop(held);
        assert!(gate
            .issue(redb::ReadableDatabase::begin_read(&database).expect("read"))
            .is_some());
    }

    /// A directory that is removed when the returned handle drops, and a redb
    /// database inside it.
    fn scratch_database() -> (ScratchDir, redb::Database) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "aster-coordination-reads-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&path).expect("create");
        let database = redb::Database::create(path.join("state.redb")).expect("create");
        (ScratchDir(path), database)
    }

    struct ScratchDir(std::path::PathBuf);

    impl Drop for ScratchDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
