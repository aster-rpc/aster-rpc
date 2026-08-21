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
//! ## The lease is about the store, not the transaction
//!
//! A builder holds its read transaction only while streaming, and then goes on
//! to write `CURRENT` into the store's directory. Ending the lease with the
//! transaction would leave that write unprotected: shutdown could drain,
//! release `LOCK`, and admit a new owner, and the old builder would then
//! publish into a directory it no longer owns. So
//! [`GuardedRead::release_transaction`] gives up the version pin and keeps the
//! lease, and publication *requires* a [`ReadLease`] in its signature.
//!
//! ## Why a lease rather than a lifetime
//!
//! A `'a`-bound read transaction would have expressed this in the type system,
//! but redb's is not one, and wrapping it in a borrow would give the transaction
//! back the property this subsystem wanted removed — it could no longer cross to
//! a blocking thread. So the count is dynamic and the guard is a value; what
//! makes it structural is that [`ReadGate::reserve`] is the only way to obtain a
//! [`ReadLease`], and neither a transaction nor a publication is reachable
//! without one.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity.
#![allow(dead_code)]

use std::sync::{Arc, Condvar, Mutex};

/// Whether the gate is handing out leases, and why not.
///
/// Three states rather than a flag, because "not admitting" had two meanings
/// that behave differently and a `bool` could not tell them apart: a
/// replacement pausing reads must resume them, and a store that has closed must
/// not — and with one flag, dropping a paused guard after the store closed
/// reopened a gate nothing was left to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admission {
    /// Reads are issued.
    Open,
    /// One [`Quiesced`] holds the gate. Exclusive: a second request waits.
    Quiesced,
    /// Terminal. Nothing reopens it.
    Closed,
}

#[derive(Debug)]
struct GateState {
    outstanding: usize,
    admission: Admission,
}

#[derive(Debug)]
struct GateInner {
    state: Mutex<GateState>,
    /// Signalled when `outstanding` reaches zero **and** when `admission`
    /// changes. Both are things a waiter here blocks on.
    changed: Condvar,
}

/// Counts the read transactions and publications in flight over `state.redb`.
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
                    admission: Admission::Open,
                }),
                changed: Condvar::new(),
            }),
        }
    }

    /// Take a lease, or refuse if the gate is not open.
    ///
    /// **Reserved before the transaction is created**, never after. Creating
    /// the transaction first left a window in which a `quiesce` could observe
    /// zero leases while a transaction already existed — and a replacement that
    /// believed no reader was open is the one case this whole module exists to
    /// prevent.
    ///
    /// Refusing rather than queueing is deliberate: the two things that shut
    /// the gate are a store that is closing and a replacement that is about to
    /// swap the file out from under this version. Neither has a transaction
    /// worth waiting for — the caller wants a read of the *current* database,
    /// and after either of those the current database is not this one.
    pub(crate) fn reserve(&self) -> Option<ReadLease> {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        if state.admission != Admission::Open {
            return None;
        }
        state.outstanding += 1;
        Some(ReadLease {
            inner: Arc::clone(&self.inner),
        })
    }

    /// Exclusive access to the database file: no read is open, and none is
    /// admitted, until the returned value is dropped.
    ///
    /// This is what install needs before renaming a new database over
    /// `state.redb`. It is exclusive — a second request waits for the first to
    /// finish rather than both believing they hold it — and it returns `None`
    /// once the store has closed, because there is then nothing to replace.
    pub(crate) fn quiesce(&self) -> Option<Quiesced> {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        loop {
            match state.admission {
                Admission::Closed => return None,
                Admission::Quiesced => {
                    state = self.inner.changed.wait(state).expect("read gate mutex")
                }
                Admission::Open => {
                    state.admission = Admission::Quiesced;
                    break;
                }
            }
        }
        while state.outstanding > 0 {
            state = self.inner.changed.wait(state).expect("read gate mutex");
        }
        Some(Quiesced {
            inner: Arc::clone(&self.inner),
        })
    }

    /// The shutdown counterpart: shut the gate permanently and wait out every
    /// lease.
    ///
    /// Terminal on purpose. A `Quiesced` outstanding at this moment finds the
    /// gate `Closed` when it drops and leaves it that way, so a replacement
    /// that was in flight cannot reopen a store that has gone.
    ///
    /// Held by [`ReadDrain`] rather than called directly, so that "the lock is
    /// not released while a read is live" is a consequence of field order in
    /// [`VoterStorage`](super::VoterStorage) — the same mechanism that already
    /// orders the actors against the lock — rather than of remembering to call
    /// something.
    fn close(&self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.admission = Admission::Closed;
        self.inner.changed.notify_all();
        while state.outstanding > 0 {
            state = self.inner.changed.wait(state).expect("read gate mutex");
        }
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
    fn admission(&self) -> Admission {
        self.inner.state.lock().expect("read gate mutex").admission
    }
}

/// One outstanding read, or one publication in progress. Dropping it is what
/// lets a waiter proceed.
#[derive(Debug)]
pub(crate) struct ReadLease {
    inner: Arc<GateInner>,
}

impl ReadLease {
    /// Open a read transaction under this lease.
    ///
    /// The only way to obtain a [`GuardedRead`], and it *consumes* the lease —
    /// so a transaction cannot be created before one is reserved. That was
    /// previously a matter of statement order, and the order was wrong in a way
    /// no test could see: with `begin_read` first, a `quiesce` could observe
    /// zero leases while a transaction already existed and return believing it
    /// had the file to itself. A race is not something to remember; it is
    /// something to make unspellable.
    pub(crate) fn read(
        self,
        database: &redb::Database,
    ) -> Result<GuardedRead, redb::TransactionError> {
        use redb::ReadableDatabase as _;

        let transaction = database.begin_read()?;
        Ok(GuardedRead {
            transaction,
            lease: self,
        })
    }
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.outstanding = state.outstanding.saturating_sub(1);
        if state.outstanding == 0 {
            self.inner.changed.notify_all();
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
    lease: ReadLease,
}

impl GuardedRead {
    /// Give up the pinned version, keep the store open.
    ///
    /// A builder is finished with the database once it has streamed the body,
    /// but not finished with the *store*: it still has to publish `CURRENT`
    /// into that directory. Releasing the transaction here lets a replacement
    /// proceed against redb while the lease continues to say this store has
    /// work outstanding.
    pub(crate) fn release_transaction(self) -> ReadLease {
        self.lease
    }
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
        let mut state = self.inner.state.lock().expect("read gate mutex");
        // Only a gate this guard actually holds is reopened. If the store
        // closed while this was outstanding, `Closed` is terminal and stays.
        if state.admission == Admission::Quiesced {
            state.admission = Admission::Open;
        }
        self.inner.changed.notify_all();
    }
}

/// Shuts the gate and waits for outstanding leases when dropped.
///
/// Its position in [`VoterStorage`](super::VoterStorage)'s field list is the
/// whole point: it is dropped before the state actor releases the `Database`
/// and long before the directory lock goes, so neither can happen while a
/// builder is still reading or still publishing.
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
        let held = gate.quiesce().expect("an open gate quiesces");
        assert_eq!(gate.outstanding(), 0);
        assert_eq!(gate.admission(), Admission::Quiesced);
        drop(held);
        assert_eq!(gate.admission(), Admission::Open, "and reads resume");
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
        let (_dir, database) = scratch_database();
        let read = guarded(&gate, &database).expect("an open gate issues");
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

    /// Releasing the transaction keeps the lease.
    ///
    /// The property publication depends on. A builder that gave up its lease
    /// with its transaction would leave the store free to close and a new owner
    /// free to open, and would then write `CURRENT` into that new owner's
    /// directory.
    #[test]
    fn releasing_a_transaction_does_not_release_the_store() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let read = guarded(&gate, &database).expect("issue");
        assert_eq!(gate.outstanding(), 1);

        let lease = read.release_transaction();
        assert_eq!(
            gate.outstanding(),
            1,
            "the version pin is gone and the store is still held"
        );
        drop(lease);
        assert_eq!(gate.outstanding(), 0);
    }

    /// A shut gate refuses rather than queues.
    #[test]
    fn a_shut_gate_issues_nothing() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let held = gate.quiesce().expect("quiesce");
        assert!(guarded(&gate, &database).is_none());
        drop(held);
        assert!(guarded(&gate, &database).is_some());
    }

    /// Quiescence is exclusive: a second request waits for the first.
    ///
    /// Two guards at once was the defect. Both callers believed they had the
    /// file to themselves, and whichever dropped first reopened admission under
    /// the other — so install could rename `state.redb` while reads were being
    /// issued against the version it was replacing.
    #[test]
    fn only_one_quiesce_holds_the_gate_at_a_time() {
        let gate = ReadGate::new();
        let first = gate.quiesce().expect("the first takes it");

        let (tx, rx) = mpsc::channel();
        let waiting = gate.clone();
        let thread = std::thread::spawn(move || {
            let second = waiting.quiesce();
            tx.send(()).expect("report");
            drop(second);
        });

        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "a second quiesce must wait rather than also succeed"
        );
        assert_eq!(gate.admission(), Admission::Quiesced);
        drop(first);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("and proceeds once the first releases");
        thread.join().expect("join");
    }

    /// A closed gate stays closed, even under a replacement already in flight.
    #[test]
    fn a_quiesce_outstanding_at_close_cannot_reopen_the_gate() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let replacing = gate.quiesce().expect("quiesce");

        drop(ReadDrain::new(gate.clone())); // the store closes
        assert_eq!(gate.admission(), Admission::Closed);

        drop(replacing);
        assert_eq!(
            gate.admission(),
            Admission::Closed,
            "a replacement guard dropping after the store closed must not \
             reopen a gate nothing is left to serve"
        );
        assert!(guarded(&gate, &database).is_none());
        assert!(gate.quiesce().is_none(), "and nothing may quiesce it again");
    }

    fn guarded(gate: &ReadGate, database: &redb::Database) -> Option<GuardedRead> {
        Some(gate.reserve()?.read(database).expect("read"))
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
