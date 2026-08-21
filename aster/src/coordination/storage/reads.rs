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
//! Exclusive access is the third thing that wait must cover, and it is the one
//! that is easy to miss: a replacement holds the database with **no** lease
//! outstanding, because waiting the count to zero is how it got there. So
//! shutdown watches the count *and* the exclusive claim; see [`GateState`].
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

use crate::coordination::model::state::LogId;

/// Whether the gate is handing out leases, and why not.
///
/// A derived view of [`GateState`], for tests and for the messages that
/// describe a refusal. It is *not* the stored form: the two reasons a gate
/// stops admitting are independent facts and were held in one slot, so setting
/// `Closed` erased the record that a replacement still held the gate — and
/// `close` then stopped waiting for it. See [`GateState`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Reads are issued.
    Open,
    /// One [`Quiesced`] holds the gate. Exclusive: a second request waits.
    Quiesced,
    /// Terminal. Nothing reopens it.
    Closed,
}

/// The two independent facts, plus the count.
///
/// `exclusive` and `closed` are orthogonal, and the orthogonality is the whole
/// correctness argument: a store can be asked to close *while* a replacement
/// holds exclusive access, and shutdown must then wait for that replacement
/// exactly as it waits for a read. One enum could not represent both at once,
/// which is how the previous version released the directory `LOCK` out from
/// under an install that was still renaming `state.redb`.
#[derive(Debug)]
struct GateState {
    outstanding: usize,
    /// A [`Quiesced`] exists, or a [`ReadGate::quiesce`] has claimed the gate
    /// and is waiting out the reads before it hands one back. Both are states
    /// in which nothing else may touch the database file.
    exclusive: bool,
    /// Terminal, set by [`ReadGate::close`] and never cleared.
    closed: bool,
}

impl GateState {
    fn admission(&self) -> Admission {
        // Closed first: it outranks everything, and a gate that is both closed
        // and still held by a departing quiescer is closed.
        if self.closed {
            Admission::Closed
        } else if self.exclusive {
            Admission::Quiesced
        } else {
            Admission::Open
        }
    }
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
                    exclusive: false,
                    closed: false,
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
        if state.admission() != Admission::Open {
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
    ///
    /// A close that arrives *while* this is waiting also yields `None`, and the
    /// claim is handed back before returning. Returning a guard in that case
    /// would be worse than useless: the caller would believe it held a database
    /// whose store was already letting the directory lock go.
    ///
    /// The obligation that comes with the guard: shutdown waits for it, so
    /// whatever holds it must not, in turn, wait on anything shutdown stops
    /// first. Replacing the file is such work; a round trip through the state
    /// actor is not.
    pub(crate) fn quiesce(&self) -> Option<Quiesced> {
        let mut state = self.inner.state.lock().expect("read gate mutex");

        // Claim exclusivity first, so that the wait below is not a wait behind
        // a stream of newly issued reads.
        while !state.closed && state.exclusive {
            state = self.inner.changed.wait(state).expect("read gate mutex");
        }
        if state.closed {
            return None;
        }
        state.exclusive = true;

        while !state.closed && state.outstanding > 0 {
            state = self.inner.changed.wait(state).expect("read gate mutex");
        }
        if state.closed {
            // Give the claim back by hand rather than through a guard that was
            // never issued: `close` is blocked on exactly this flag, and would
            // otherwise wait for a `Quiesced` that is not going to exist.
            state.exclusive = false;
            self.inner.changed.notify_all();
            return None;
        }
        Some(Quiesced {
            inner: Arc::clone(&self.inner),
        })
    }

    /// The shutdown counterpart: shut the gate permanently and wait out every
    /// lease **and** any exclusive holder.
    ///
    /// Waiting for the quiescer is not symmetry for its own sake. Exclusive
    /// access is what install renames `state.redb` under, and it is held
    /// without any lease outstanding — by construction, since it waits for the
    /// count to reach zero. A close that only counted leases therefore saw an
    /// idle gate at the exact moment the database file was being replaced, and
    /// released the directory `LOCK` under it.
    ///
    /// Terminal on purpose: `closed` is never cleared, so a replacement that
    /// was in flight cannot reopen a store that has gone.
    ///
    /// Held by [`ReadDrain`] rather than called directly, so that "the lock is
    /// not released while a read is live" is a consequence of field order in
    /// [`VoterStorage`](super::VoterStorage) — the same mechanism that already
    /// orders the actors against the lock — rather than of remembering to call
    /// something.
    fn close(&self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.closed = true;
        self.inner.changed.notify_all();
        while state.outstanding > 0 || state.exclusive {
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
    pub(crate) fn admission(&self) -> Admission {
        self.inner
            .state
            .lock()
            .expect("read gate mutex")
            .admission()
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

    /// Which applied version this transaction contains, read from `STATE_META`
    /// **inside it**.
    ///
    /// A linearizable read observes the version its transaction pins, and that
    /// is not the version the barrier established. `ensure_linearizable` waits
    /// for the state machine to reach its read log id and returns; by the time
    /// a handler holds a transaction, later batches may already have committed.
    /// So the pinned version is at or past the barrier and is never reliably
    /// equal to it -- which is still linearizable, but only if the pinned
    /// version is the one checked, and only if authorization and data then come
    /// from this same transaction.
    ///
    /// Which is why this reads the row here rather than reporting what the
    /// actor holds. [`StateHandle::applied_state`](super::machine::StateHandle)
    /// answers from the actor's in-memory state, and that state can be *newer*
    /// than any outstanding transaction: satisfying a barrier from it would
    /// confirm a version this read will never observe.
    pub(crate) fn pinned(&self) -> super::error::StorageResult<PinnedVersion> {
        super::rows::pinned_applied(&self.transaction).map(PinnedVersion)
    }
}

/// The applied log id one pinned read transaction contains.
///
/// A newtype rather than a bare `Option<LogId>` so that
/// [`against`](PinnedVersion::against) can only be asked about a version some
/// transaction actually holds. The obvious other source -- the actor's applied
/// state -- has the same Rust type and the wrong meaning, and substituting it
/// is the one mistake this whole API exists to stop. Different type, compile
/// error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PinnedVersion(Option<LogId>);

impl PinnedVersion {
    pub(crate) fn log_id(&self) -> Option<&LogId> {
        self.0.as_ref()
    }

    /// How this version stands against a linearizable barrier.
    ///
    /// `barrier` is what `Raft::ensure_linearizable` returned, converted to the
    /// model's log id. `None` means the barrier established that nothing has
    /// been applied group-wide, which every version is at or past.
    pub(crate) fn against(&self, barrier: Option<&LogId>) -> BarrierFit {
        let Some(barrier) = barrier else {
            return BarrierFit::Satisfied;
        };
        let Some(pinned) = self.0.as_ref() else {
            return BarrierFit::Behind;
        };
        // Explicitly by index. `LogId` has no `Ord` for exactly this reason: a
        // derived one would compare `term` first, which orders two entries by
        // the leader that wrote them rather than by position in the log.
        match pinned.index.cmp(&barrier.index) {
            std::cmp::Ordering::Less => BarrierFit::Behind,
            std::cmp::Ordering::Greater => BarrierFit::Satisfied,
            std::cmp::Ordering::Equal if pinned == barrier => BarrierFit::Satisfied,
            std::cmp::Ordering::Equal => BarrierFit::Diverged,
        }
    }
}

/// How a pinned version stands against a linearizable barrier.
///
/// Three outcomes rather than a bool, because "behind" and "disagrees" are not
/// the same failure and must not be retried the same way. A transaction older
/// than the barrier is ordinary: the read takes a newer one. Two *different*
/// log ids at one applied index mean this voter applied an entry the barrier's
/// leader did not -- divergent history, which §5.3.1 makes fatal. Folded into
/// one boolean, the second is indistinguishable from the first and a handler
/// would retry its way around a split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BarrierFit {
    /// At or past the barrier: this transaction may serve the read.
    Satisfied,
    /// Older than the barrier.
    Behind,
    /// Same applied index, different log id.
    Diverged,
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
    /// Gives the claim back, and nothing else.
    ///
    /// Reopening is not a decision made here: `closed` is a separate fact this
    /// does not touch, so a gate the store has already shut stays shut and a
    /// gate it has not reopens. Dropping this is also what releases a `close`
    /// that has been waiting for exclusive access to end.
    fn drop(&mut self) {
        let mut state = self.inner.state.lock().expect("read gate mutex");
        state.exclusive = false;
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

    /// Closing waits for exclusive access to end, and the gate is closed for
    /// good afterwards.
    ///
    /// The first half is the defect: a quiescer holds the gate with *no* lease
    /// outstanding — waiting the leases out is how it got the gate — so a close
    /// that counted only leases saw an idle store and returned. In
    /// [`VoterStorage`](super::VoterStorage) that means the directory `LOCK`
    /// goes while install is part-way through replacing `state.redb`, and a
    /// second voter opens onto a half-replaced database.
    #[test]
    fn a_close_waits_for_the_quiescer_and_then_stays_shut() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let replacing = gate.quiesce().expect("quiesce");
        assert_eq!(gate.outstanding(), 0, "a quiescer holds no lease");

        let (tx, rx) = mpsc::channel();
        let closing = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                drop(ReadDrain::new(gate));
                tx.send(()).expect("report");
            })
        };

        assert!(
            rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "closing must not complete while a replacement holds the database"
        );
        drop(replacing);
        rx.recv_timeout(Duration::from_secs(10))
            .expect("and completes once it lets go");
        closing.join().expect("join");

        assert_eq!(
            gate.admission(),
            Admission::Closed,
            "a replacement guard dropping after the store closed must not \
             reopen a gate nothing is left to serve"
        );
        assert!(guarded(&gate, &database).is_none());
        assert!(gate.quiesce().is_none(), "and nothing may quiesce it again");
    }

    /// A quiesce that is waiting out a read when the store closes gets nothing.
    ///
    /// The other interleaving. Here the claim is already made and the wait is
    /// on a live reader; the close arrives, and when the reader finally goes
    /// both wake. Handing the caller exclusive access at that point would give
    /// install a database whose store was already releasing the directory lock.
    #[test]
    fn a_quiesce_waiting_when_the_store_closes_gets_nothing() {
        let gate = ReadGate::new();
        let (_dir, database) = scratch_database();
        let read = guarded(&gate, &database).expect("an open gate issues");

        let (answers, answered) = mpsc::channel();
        let quiescing = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                let held = gate.quiesce();
                answers.send(held.is_some()).expect("report");
            })
        };
        wait_for(&gate, Admission::Quiesced);

        let (closed, closes) = mpsc::channel();
        let closing = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                drop(ReadDrain::new(gate));
                closed.send(()).expect("report");
            })
        };
        assert!(
            closes.recv_timeout(Duration::from_millis(250)).is_err(),
            "the close is itself waiting for the outstanding read"
        );

        drop(read);
        assert!(
            !answered
                .recv_timeout(Duration::from_secs(10))
                .expect("the quiesce resolves"),
            "a quiesce that was waiting when the store closed must not return \
             exclusive access to a store that is going away"
        );
        closes
            .recv_timeout(Duration::from_secs(10))
            .expect("and the close is not left waiting for a guard that was never issued");
        quiescing.join().expect("join");
        closing.join().expect("join");
    }

    /// The barrier rule, in all four shapes it has to answer.
    ///
    /// Ahead is the ordinary case and the one a bare equality check would get
    /// wrong: a read handler holds its transaction *after* the barrier
    /// resolved, so later batches have usually committed by then.
    #[test]
    fn a_pinned_version_is_measured_against_the_barrier_by_index() {
        let id = |index| LogId::new(1, [0x11; 32], index);
        let pinned = |version: Option<LogId>| PinnedVersion(version);

        assert_eq!(
            pinned(Some(id(7))).against(Some(&id(5))),
            BarrierFit::Satisfied,
            "a transaction past the barrier serves the read"
        );
        assert_eq!(
            pinned(Some(id(5))).against(Some(&id(5))),
            BarrierFit::Satisfied
        );
        assert_eq!(
            pinned(Some(id(3))).against(Some(&id(5))),
            BarrierFit::Behind,
            "an older transaction cannot answer a newer barrier"
        );
        assert_eq!(
            pinned(None).against(Some(&id(5))),
            BarrierFit::Behind,
            "and neither can one that has applied nothing"
        );
        assert_eq!(
            pinned(None).against(None),
            BarrierFit::Satisfied,
            "a barrier that established nothing is met by everything"
        );
        assert_eq!(pinned(Some(id(5))).against(None), BarrierFit::Satisfied);
    }

    /// Two different entries at one applied index is not staleness.
    ///
    /// The case the three-way answer exists for. A `bool` would report this
    /// exactly as it reports "behind", and the handler would retry -- around a
    /// divergence, forever, serving nothing and reporting nothing wrong.
    #[test]
    fn an_equal_index_under_another_leader_is_divergence_not_staleness() {
        let barrier = LogId::new(1, [0x11; 32], 5);

        assert_eq!(
            PinnedVersion(Some(LogId::new(1, [0x22; 32], 5))).against(Some(&barrier)),
            BarrierFit::Diverged,
            "same term and index, a different leader: different entries"
        );
        assert_eq!(
            PinnedVersion(Some(LogId::new(2, [0x11; 32], 5))).against(Some(&barrier)),
            BarrierFit::Diverged,
            "and a different term is the same kind of disagreement"
        );
    }

    fn guarded(gate: &ReadGate, database: &redb::Database) -> Option<GuardedRead> {
        Some(gate.reserve()?.read(database).expect("read"))
    }

    /// Spin until another thread has moved the gate into `wanted`.
    ///
    /// Bounded so that a regression fails the test rather than hanging the
    /// suite; the states this waits for are set microseconds after the thread
    /// starts.
    fn wait_for(gate: &ReadGate, wanted: Admission) {
        for _ in 0..10_000 {
            if gate.admission() == wanted {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        panic!("the gate never reached {wanted:?}");
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
