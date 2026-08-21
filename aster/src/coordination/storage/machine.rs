//! `RaftStateMachine` over `state.redb` (§11.3).
//!
//! ## The rule this module exists to hold
//!
//! One apply batch is one redb transaction, and **memory never moves ahead of
//! disk**. The actor mutates a private clone of its state, commits every
//! entry's rows in a single transaction, and only then publishes that clone and
//! answers OpenRaft. A commit that fails leaves the published state exactly
//! where it was and latches the store fatal; the working copy is dropped
//! unread.
//!
//! Doing it the other way round is the failure §4.4 forbids outright, and it is
//! silent: every read in the process would see the newer state, and only a
//! restart — or a follower asking for a snapshot — would find the disk behind.
//!
//! ## Why OpenRaft's own entries go through the same guard
//!
//! `Blank` and `Membership` are not `CommandV1`s, so they do not reach
//! [`apply_command`](crate::coordination::model::apply::apply_command). §10.2's
//! delivery rule is about delivery rather than about what was delivered, so
//! they share
//! [`guard_delivery`](crate::coordination::model::apply::guard_delivery)
//! instead of getting a second copy of it. Neither writes an audit row or runs
//! an application transition: a `Blank` advances `last_applied` and nothing
//! else, and a `Membership` additionally records the exact `StoredMembership`.
//! Both answer [`CommandReplyV1::NonCommand`], because OpenRaft requires one
//! response per applied entry whatever the entry was.
//!
//! ## One rule fixed in advance
//!
//! Every encode on the apply path goes through
//! [`codec::encode_committed`](super::codec::encode_committed), never
//! [`encode_bounded`](super::codec::encode_bounded) — enforced structurally by
//! `apply_path_encodes_are_fatal_not_retryable` in [`super`].
//!
//! The two differ only in classification, and the difference matters exactly
//! here. A retryable `EncodeFailed` is correct for a value a *caller* offered:
//! refusing it rejects a request and writes nothing. Once an entry is
//! committed the group has already decided it applies, so a state machine that
//! cannot encode the result has no retry available — reporting one would let
//! the runtime carry on with memory ahead of disk, which §4.4 forbids
//! outright.

// See the `storage` module docs: nothing in a shipping build reaches
// `VoterStorage::open` until Slice 3 supplies a verified identity.
#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use openraft::storage::RaftStateMachine;
use openraft::{
    EntryPayload, LogId as RaftLogId, RaftSnapshotBuilder, Snapshot, SnapshotMeta,
    StorageError as RaftStorageError, StorageIOError, StoredMembership,
};
use redb::Database;
use tokio::sync::{mpsc, oneshot};

use super::actor::ActorShutdown;
use super::error::{Db, StorageError, StorageResult};
use super::reads::{GuardedRead, Quiesced, ReadDrain, ReadGate, ReadLease};
use super::rows::{self, DurableState};
use super::schema::StorageLayout;
use super::snapshot::{self, Publication, SnapshotCatalog, SnapshotFile};
use super::CoordinationSnapshotFile;
use crate::coordination::contract::{CommandReplyV1, NonCommandReplyV1};
use crate::coordination::ids::CoordinationNodeId;
use crate::coordination::model::apply::guard_delivery;
use crate::coordination::model::boundary::apply_wire_command_recording;
use crate::coordination::model::state::{ChangeSet, LogId};
use crate::coordination::typeconfig::{CoordinationNode, CoordinationTypeConfig};

/// Depth of the state actor's command queue.
///
/// Bounded for the same reason the log's is: an unbounded queue in front of a
/// slow disk converts back pressure into unbounded memory growth. Smaller than
/// the log's because each item is a whole apply batch rather than one command.
const STATE_ACTOR_QUEUE: usize = 16;

/// The applied membership OpenRaft asks for at startup.
type Applied = (
    Option<RaftLogId<CoordinationNodeId>>,
    StoredMembership<CoordinationNodeId, CoordinationNode>,
);

type Entry = openraft::Entry<CoordinationTypeConfig>;
type Reply<T> = oneshot::Sender<StorageResult<T>>;

// ── Actor protocol ───────────────────────────────────────────────────────────

pub(crate) enum StateCommand {
    AppliedState(Reply<Applied>),
    Apply {
        entries: Vec<Entry>,
        reply: Reply<Vec<CommandReplyV1>>,
    },
    /// Open a snapshot-consistent read transaction and hand it out.
    ///
    /// The transaction is created **by the actor** so it can only observe a
    /// committed batch — the actor is between batches whenever it services a
    /// command — and then leaves, because §11.4 requires the body to stream
    /// without blocking the writer. redb's MVCC is what makes both true at
    /// once: the version this pins does not move, and later applies do not wait
    /// for it.
    ReadTransaction(Reply<GuardedRead>),
    /// Hand out a copy of the published state.
    ///
    /// Test-only, and deliberately so. It clones the whole state — up to 192
    /// MiB under §12.1 — which was acceptable only while it stood in for the
    /// snapshot builder. [`StateCommand::ReadTransaction`] is that builder now,
    /// so no production path copies the state to read it.
    #[cfg(test)]
    Snapshot(Reply<Box<DurableState>>),
    Shutdown,
    /// Force a fatal failure through the ordinary serve path.
    #[cfg(test)]
    InjectFatal(Reply<()>),
    /// Arm the commit failure described on [`StateActor::fail_next_commit`].
    #[cfg(test)]
    FailNextCommit(Reply<()>),
    /// Block the actor until the test releases it.
    ///
    /// Without this there is no way to enqueue work *behind* a known failure:
    /// [`StateHandle::call`] checks the fatal latch before sending, so by the
    /// time a test can react to a failure it can no longer put anything in the
    /// queue -- and the drain below has nothing to refuse. Pausing the actor
    /// first is what makes the queue observable.
    #[cfg(test)]
    Pause(std::sync::mpsc::Receiver<()>),
}

/// Whether the actor may keep serving after one command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Continue,
    Fatal,
}

/// The single-writer thread that owns `state.redb`.
struct StateActor {
    database: Database,
    /// Counts the read transactions this actor has handed out.
    reads: ReadGate,
    /// The **published** state: what is on disk, and nothing more.
    ///
    /// `Option` because an apply batch *moves* it out. The state is a whole
    /// group -- up to 192 MiB of resource values under §12.1 -- so cloning it
    /// per batch would copy the entire store to apply one command. Taking it
    /// instead makes the batch's working value the only one that exists, and
    /// `rows::commit` hands it back only on success.
    ///
    /// That also makes "every failure after the take is fatal" structural
    /// rather than a property of the error classifier: an error path leaves
    /// this `None`, and every reader below answers `Fatal` when it is. There is
    /// no state to serve, because the only copy was the one the failed batch
    /// was holding.
    state: Option<DurableState>,
    fatal: Arc<AtomicBool>,
    /// Fail the next batch's commit, after its entries have already mutated
    /// the working copy.
    ///
    /// A deliberate test seam, and the only way to reach the case that matters
    /// most here: a commit that fails *after* the transition ran. Faking it in
    /// a harness would prove the harness; this routes through the real `apply`,
    /// so the working copy really has been mutated by the time the failure
    /// happens and "nothing was published" is a statement about production
    /// code.
    #[cfg(test)]
    fail_next_commit: bool,
}

impl StateActor {
    fn run(mut self, mut rx: mpsc::Receiver<StateCommand>, stop: Arc<AtomicBool>) {
        while let Some(command) = rx.blocking_recv() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let outcome = match command {
                StateCommand::Shutdown => break,
                StateCommand::AppliedState(reply) => self.serve(reply, StateActor::applied_state),
                StateCommand::Apply { entries, reply } => self.serve(reply, |a| a.apply(&entries)),
                StateCommand::ReadTransaction(reply) => {
                    self.serve(reply, StateActor::read_transaction)
                }
                #[cfg(test)]
                StateCommand::Snapshot(reply) => self.serve(reply, |a| a.published().map(Box::new)),
                #[cfg(test)]
                StateCommand::FailNextCommit(reply) => self.serve(reply, |a| {
                    a.fail_next_commit = true;
                    Ok(())
                }),
                #[cfg(test)]
                StateCommand::InjectFatal(reply) => self.serve(reply, |_| {
                    Err(StorageError::corrupt(
                        Db::State,
                        "CoordinationMetaV1",
                        "injected durability failure",
                    ))
                }),
                #[cfg(test)]
                StateCommand::Pause(gate) => {
                    let _ = gate.recv();
                    Outcome::Continue
                }
            };
            if outcome == Outcome::Fatal {
                self.drain_fatal(&mut rx);
                break;
            }
        }
    }

    /// Run one command, latch fatality **before** replying, and report which it
    /// was.
    ///
    /// The ordering is the point, and it is the same one the log actor holds:
    /// classifying in the caller leaves a window in which a batch queued behind
    /// a failed commit is serviced and acknowledged.
    fn serve<T, F>(&mut self, reply: Reply<T>, run: F) -> Outcome
    where
        F: FnOnce(&mut Self) -> StorageResult<T>,
    {
        let result = run(self);
        let outcome = match &result {
            Err(err) if err.puts_durability_in_question() => {
                self.fatal.store(true, Ordering::SeqCst);
                Outcome::Fatal
            }
            _ => Outcome::Continue,
        };
        let _ = reply.send(result);
        outcome
    }

    /// Refuse everything already queued behind a failure.
    ///
    /// Those commands were enqueued while the store still looked healthy, and
    /// serving them would acknowledge writes after durability became uncertain.
    fn drain_fatal(&mut self, rx: &mut mpsc::Receiver<StateCommand>) {
        rx.close();
        while let Some(command) = rx.blocking_recv() {
            match command {
                StateCommand::Shutdown => break,
                StateCommand::AppliedState(reply) => fail(reply),
                StateCommand::Apply { reply, .. } => fail(reply),
                StateCommand::ReadTransaction(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::Snapshot(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::InjectFatal(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::FailNextCommit(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::Pause(_) => {}
            }
        }
    }

    /// The published state, or `Fatal` if a batch took it and never gave it
    /// back.
    #[cfg(test)]
    fn published(&self) -> StorageResult<DurableState> {
        self.state.clone().ok_or(StorageError::Fatal)
    }

    /// A read transaction over the committed state.
    ///
    /// Refused while the state is taken, for the same reason every other reader
    /// is: a batch is holding the only copy, and whether its transaction
    /// commits is not yet known. The check is on the *in-memory* state rather
    /// than on redb, because redb would happily hand out the last committed
    /// version and that version is about to be either superseded or the last
    /// thing this store ever agreed on.
    fn read_transaction(&mut self) -> StorageResult<GuardedRead> {
        if self.state.is_none() {
            return Err(StorageError::Fatal);
        }
        // The lease comes first, and `ReadLease::read` is what makes that a
        // property of the types rather than of these two statements' order: a
        // transaction created before a lease existed is a reader a replacement
        // could not see, and the reversed order looked perfectly correct.
        //
        // A shut gate means this store is closing or its file is about to be
        // swapped; either way the version about to be pinned is not the one the
        // caller wants, so this refuses rather than queues.
        self.reads
            .reserve()
            .ok_or(StorageError::ActorStopped { db: Db::State })?
            .read(&self.database)
            .map_err(|source| StorageError::redb(Db::State, source))
    }

    fn applied_state(&mut self) -> StorageResult<Applied> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok((
            state.model.last_applied.as_ref().map(Into::into),
            state.membership.clone(),
        ))
    }

    /// Apply one batch: every entry into a private copy, one transaction, then
    /// publish.
    fn apply(&mut self, entries: &[Entry]) -> StorageResult<Vec<CommandReplyV1>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // The private working value, **moved** out of the actor rather than
        // copied from it. Nothing outside this function can observe it, and if
        // anything below fails it is dropped rather than published -- which is
        // what keeps memory from running ahead of disk, and now also means
        // there is no published state left to serve until a commit returns one.
        let mut working = self.state.take().ok_or(StorageError::Fatal)?;
        let mut changes = ChangeSet::default();
        let mut replies = Vec::with_capacity(entries.len());
        let group = working.model.authority.group_id;

        for entry in entries {
            let log_id = LogId::from(&entry.log_id);
            let reply = match &entry.payload {
                // §11.3: a blank advances `last_applied` and nothing else. No
                // audit row -- there is no command to audit -- and no
                // transition.
                EntryPayload::Blank => {
                    guard_delivery(&working.model, log_id).map_err(fatal_fault)?;
                    working.model.last_applied = Some(log_id);
                    CommandReplyV1::NonCommand(NonCommandReplyV1 {})
                }
                // A membership entry records the exact `StoredMembership`
                // OpenRaft asked for. The model's member set and membership log
                // id are **derived** from it rather than stored beside it: only
                // the membership itself is persisted (§11.3), so anything kept
                // separately would be re-derived differently at the next open.
                EntryPayload::Membership(membership) => {
                    guard_delivery(&working.model, log_id).map_err(fatal_fault)?;
                    let projection = rows::project_membership(membership);
                    // The writer must not write what the reader refuses. §9's
                    // envelope is enforced at admission, so a membership over
                    // it means a controller ignored that -- and persisting it
                    // would leave a store that applied cleanly and cannot
                    // reopen, which is the worst place to find out.
                    if projection.learners.len()
                        > crate::coordination::model::shape::MAX_LEARNER_ENVELOPE
                    {
                        return Err(StorageError::corrupt(
                            Db::State,
                            "membership",
                            format!(
                                "a membership entry names {} learners, over the {}-learner \
                                 envelope; the store would not reopen",
                                projection.learners.len(),
                                crate::coordination::model::shape::MAX_LEARNER_ENVELOPE
                            ),
                        ));
                    }
                    working.membership =
                        StoredMembership::new(Some(entry.log_id), membership.clone());
                    working.model.membership = projection;
                    working.model.membership_log_id = Some(log_id);
                    working.model.last_applied = Some(log_id);
                    CommandReplyV1::NonCommand(NonCommandReplyV1 {})
                }
                EntryPayload::Normal(command) => apply_wire_command_recording(
                    &mut working.model,
                    log_id,
                    &group,
                    command,
                    &mut changes,
                )
                .map_err(fatal_fault)?,
            };
            replies.push(reply);
        }

        // One transaction for the whole batch, including the meta row that
        // carries `last_applied`, the counters, the audit floor and the
        // membership.
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_commit) {
            return Err(StorageError::corrupt(
                Db::State,
                "CoordinationMetaV1",
                "injected commit failure",
            ));
        }
        // Publication and the commit are one statement, and deliberately: the
        // working copy is *moved* into `rows::commit` and comes back only if
        // the commit returned, so there is no committed-looking state to
        // publish early. The ordering is unobservable from outside -- a commit
        // failure latches fatal before it replies, and a fatal store answers
        // nothing further -- so it is carried by the signature rather than by a
        // test whose name would imply it had been checked.
        self.state = Some(rows::commit(&self.database, working, &changes)?);
        Ok(replies)
    }
}

/// A model fault is fatal for the store.
///
/// §5.3.1 and §10.2 both make their conditions fatal rather than
/// client-visible: an entry this voter cannot interpret, and an entry at or
/// below `last_applied`, are each evidence that this node's history and the
/// group's have diverged. Continuing would mean writing state that disagrees
/// with a peer's.
fn fatal_fault(fault: crate::coordination::model::apply::Fault) -> StorageError {
    StorageError::corrupt(Db::State, "apply", format!("{fault:?}"))
}

fn fail<T>(reply: Reply<T>) {
    let _ = reply.send(Err(StorageError::Fatal));
}

// ── Handle ───────────────────────────────────────────────────────────────────

/// The state actor's shutdown guard.
pub(crate) type StateShutdown = ActorShutdown<StateCommand>;

#[derive(Clone)]
pub(crate) struct StateHandle {
    tx: mpsc::Sender<StateCommand>,
    fatal: Arc<AtomicBool>,
}

impl std::fmt::Debug for StateHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateHandle")
            .field("fatal", &self.fatal.load(Ordering::SeqCst))
            .finish()
    }
}

impl StateHandle {
    /// Start the actor on an already-loaded state.
    ///
    /// The state is loaded and validated by the caller, **before** this exists,
    /// for the same reason the log's validation is: a store published first and
    /// validated later has already told the group it is up by the time the
    /// first apply finds the corruption. The caller does it rather than this
    /// function because `CURRENT` has to be checked against the loaded state,
    /// and that check also belongs before any actor.
    pub(crate) fn spawn(
        database: Database,
        state: DurableState,
        reads: ReadGate,
    ) -> StorageResult<(Self, StateShutdown)> {
        let (tx, rx) = mpsc::channel(STATE_ACTOR_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let actor_stop = Arc::clone(&stop);
        let fatal = Arc::new(AtomicBool::new(false));
        let actor_fatal = Arc::clone(&fatal);
        let thread = std::thread::Builder::new()
            .name("aster-coordination-state".into())
            .spawn(move || {
                StateActor {
                    database,
                    reads,
                    state: Some(state),
                    fatal: actor_fatal,
                    #[cfg(test)]
                    fail_next_commit: false,
                }
                .run(rx, actor_stop)
            })
            .map_err(|source| StorageError::ActorSpawn {
                db: Db::State,
                source,
            })?;

        Ok((
            Self {
                tx: tx.clone(),
                fatal,
            },
            ActorShutdown::new(tx, stop, thread, || StateCommand::Shutdown),
        ))
    }

    pub(crate) fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::SeqCst)
    }

    /// The same refusal every actor command makes, for work that does not go
    /// through the actor.
    ///
    /// Snapshot publication writes files rather than rows, so it never reaches
    /// [`call`](Self::call) and would otherwise be the one path that keeps
    /// serving after the store has stopped being able to say what is durable.
    pub(crate) fn check_serviceable(&self) -> StorageResult<()> {
        if self.is_fatal() {
            return Err(StorageError::Fatal);
        }
        Ok(())
    }

    pub(crate) fn mark_fatal(&self) {
        self.fatal.store(true, Ordering::SeqCst);
    }

    async fn call<T, F>(&self, build: F) -> StorageResult<T>
    where
        F: FnOnce(Reply<T>) -> StateCommand,
    {
        if self.is_fatal() {
            return Err(StorageError::Fatal);
        }
        let (reply_tx, reply_rx) = oneshot::channel();
        if self.tx.send(build(reply_tx)).await.is_err() {
            self.mark_fatal();
            return Err(StorageError::ActorStopped { db: Db::State });
        }
        match reply_rx.await {
            Ok(result) => result,
            Err(_) => {
                self.mark_fatal();
                Err(StorageError::ActorStopped { db: Db::State })
            }
        }
    }

    /// A snapshot-consistent read transaction over `state.redb`.
    pub(crate) async fn read_transaction(&self) -> StorageResult<GuardedRead> {
        self.call(StateCommand::ReadTransaction).await
    }

    /// A copy of the published state, for tests that compare memory with disk.
    #[cfg(test)]
    pub(crate) async fn published(&self) -> StorageResult<Box<DurableState>> {
        self.call(StateCommand::Snapshot).await
    }

    #[cfg(test)]
    async fn inject_fatal(&self) -> StorageResult<()> {
        self.call(StateCommand::InjectFatal).await
    }

    #[cfg(test)]
    async fn fail_next_commit(&self) -> StorageResult<()> {
        self.call(StateCommand::FailNextCommit).await
    }

    /// Block the actor and return the release handle.
    #[cfg(test)]
    async fn pause(&self) -> std::sync::mpsc::Sender<()> {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();
        self.tx
            .send(StateCommand::Pause(gate_rx))
            .await
            .expect("the actor is running");
        gate_tx
    }

    /// Enqueue a command without awaiting its reply, bypassing the caller-side
    /// latch.
    ///
    /// The ordering guarantee this exists for: with the actor paused, two
    /// successive sends land in the channel in exactly the order issued, which
    /// is what makes "queued behind a failure" a testable statement rather than
    /// a race.
    #[cfg(test)]
    async fn enqueue<T, F>(&self, build: F) -> oneshot::Receiver<StorageResult<T>>
    where
        F: FnOnce(Reply<T>) -> StateCommand,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(build(reply_tx))
            .await
            .expect("the actor is running");
        reply_rx
    }
}

// ── OpenRaft error mapping ───────────────────────────────────────────────────

fn io_state(err: StorageError) -> RaftStorageError<CoordinationNodeId> {
    RaftStorageError::IO {
        source: StorageIOError::write_state_machine(openraft::AnyError::new(&err)),
    }
}

fn io_read_state(err: StorageError) -> RaftStorageError<CoordinationNodeId> {
    RaftStorageError::IO {
        source: StorageIOError::read_state_machine(openraft::AnyError::new(&err)),
    }
}

// ── The store ────────────────────────────────────────────────────────────────

/// `RaftStateMachine` over `state.redb`.
#[derive(Clone, Debug)]
pub(crate) struct CoordinationStateMachine {
    handle: StateHandle,
    layout: StorageLayout,
    catalog: SnapshotCatalog,
    /// The published `CURRENT`, as validated at open and maintained since.
    ///
    /// Held rather than re-read because `get_current_snapshot` is on the
    /// replication path: re-reading would hash and decode the entire body on
    /// every follower request, which is work proportional to the state for a
    /// question already answered. The disk marker stays authoritative — this is
    /// only ever written from a publication that has already landed.
    current: Arc<Mutex<Option<SnapshotFile>>>,
    /// The same gate the actor issues read leases from.
    ///
    /// Held here so exclusive access is reachable without the actor — which
    /// shutdown needs, since it runs after the actor has stopped.
    ///
    /// It used to say the actor *could not* wait out readers, because a reader
    /// might be waiting on the actor. That is not true, and the install path
    /// depends on it not being true: a lease holder makes exactly one actor
    /// call, the one that hands it the transaction, and everything afterwards —
    /// streaming the body, releasing the transaction, publishing `CURRENT` —
    /// runs off the actor entirely. So the actor can block in `quiesce` and
    /// every outstanding holder can still finish and let go.
    ///
    /// `a_lease_holder_never_waits_on_the_actor` is that premise, tested rather
    /// than asserted: it pauses the actor and drives a complete build and
    /// publication through to release with the actor unable to answer anything.
    reads: ReadGate,
}

impl CoordinationStateMachine {
    pub(crate) fn new(
        database: Database,
        layout: StorageLayout,
    ) -> StorageResult<(Self, StateShutdown, ReadDrain)> {
        // Both before the actor exists. The state load validates every §11.3
        // row; `CURRENT` is then checked against that state, because a marker
        // is only meaningful relative to the store it sits beside. A store that
        // announced itself first would be offering a snapshot to followers by
        // the time anything noticed either was wrong.
        let state = rows::load(&database)?;
        let current = snapshot::load_current(&layout, &database, &state)?;

        let reads = ReadGate::new();
        let (handle, shutdown) = StateHandle::spawn(database, state, reads.clone())?;
        Ok((
            Self {
                handle,
                layout,
                catalog: SnapshotCatalog::new(),
                current: Arc::new(Mutex::new(current)),
                reads: reads.clone(),
            },
            shutdown,
            ReadDrain::new(reads),
        ))
    }

    pub(crate) fn handle(&self) -> &StateHandle {
        &self.handle
    }

    /// Exclusive access to `state.redb`: no read is open, and none is issued,
    /// until the returned guard drops.
    ///
    /// What install replaces the database file under. `None` means the store is
    /// closing, and a caller that gets it must not touch the directory — the
    /// shutdown it lost to is releasing the lock.
    pub(crate) fn quiesce_reads(&self) -> Option<Quiesced> {
        self.reads.quiesce()
    }

    /// Apply §11.4's publication rule to a freshly built body, and lease
    /// whatever is current afterwards.
    ///
    /// Separate from `build_snapshot` so that an out-of-order builder is a
    /// state a test can reach without a seam: taking a read transaction,
    /// applying more entries, and only then streaming that transaction is a
    /// builder that genuinely finished behind the current snapshot, and it can
    /// be handed straight to this.
    ///
    /// Returns a lease rather than a file because the two cannot be separated;
    /// see [`lease`](Self::lease).
    ///
    /// ## Why it demands a `ReadLease`
    ///
    /// Publication writes `CURRENT` into the store's directory, and it happens
    /// *after* the builder has finished with its read transaction. Ending the
    /// lease with the transaction left that write unguarded: shutdown could
    /// drain, release `LOCK`, and admit a new owner, and a builder that was
    /// merely slow would then overwrite that new owner's marker with a snapshot
    /// of a database it no longer had. Requiring the lease here makes the gap
    /// inexpressible rather than merely tested — the borrow must outlive the
    /// call, so a builder cannot drop it early and still reach this.
    ///
    /// A publication failure latches the store fatal for the same reason a
    /// failed commit does. `publish_current` can replace the marker and then
    /// fail to make the replacement durable, at which point neither disk nor
    /// memory can be trusted to say what is published; carrying on would let a
    /// later publication decide monotonicity from metadata that is already
    /// wrong.
    pub(crate) fn publish(
        &self,
        _lease: &ReadLease,
        built: SnapshotFile,
    ) -> StorageResult<Snapshot<CoordinationTypeConfig>> {
        self.under_current(|this, current| this.publish_inner(current, built))
    }

    /// The publication critical section: refuse, act, and latch under one lock.
    ///
    /// The three were separate steps, and the gap between them was reachable.
    /// Two publishers could both pass the fatal check before either took the
    /// mutex; the first would replace `CURRENT`, fail to fsync, and latch —
    /// after the second had already been admitted and was deciding
    /// monotonicity against metadata that no longer described the disk. Holding
    /// the mutex across all three makes "was this store serviceable when I
    /// decided?" and "is it serviceable now?" the same question.
    ///
    /// Reading is inside it too. `get_current_snapshot` is a report about the
    /// same marker, so a report issued between a failed replacement and its
    /// latch would name a snapshot the store can no longer vouch for.
    #[allow(clippy::result_large_err)]
    fn under_current<T>(
        &self,
        op: impl FnOnce(&Self, &mut Option<SnapshotFile>) -> StorageResult<T>,
    ) -> StorageResult<T> {
        let mut current = self.current.lock().expect("the current-snapshot mutex");
        self.handle.check_serviceable()?;
        let result = op(self, &mut current);
        if let Err(err) = &result {
            if err.puts_durability_in_question() {
                self.handle.mark_fatal();
            }
        }
        result
    }

    #[allow(clippy::result_large_err)]
    fn publish_inner(
        &self,
        current: &mut Option<SnapshotFile>,
        built: SnapshotFile,
    ) -> StorageResult<Snapshot<CoordinationTypeConfig>> {
        let serving =
            match snapshot::decide_publication(&built.meta, current.as_ref().map(|f| &f.meta)) {
                Publication::Publish => {
                    snapshot::publish_current(&self.layout, &built.meta)?;
                    if let Some(previous) = current.replace(built.clone()) {
                        self.catalog
                            .retire(&previous.meta.snapshot_id, &previous.path);
                    }
                    built
                }
                // The body is already on disk under its digest name, so this
                // republishes the marker and nothing else. That is the repair
                // §11.4 requires after a lost acknowledgement, not a no-op.
                Publication::Idempotent => {
                    snapshot::publish_current(&self.layout, &built.meta)?;
                    built
                }
                // A builder that finished behind the current snapshot. The marker
                // does not move and the file goes, which is what "discarded" has to
                // mean if it is not to accumulate.
                Publication::Discard => {
                    self.catalog.retire(&built.meta.snapshot_id, &built.path);
                    current
                        .clone()
                        .expect("only a published snapshot can be ahead of a candidate")
                }
                Publication::Fatal(detail) => {
                    return Err(StorageError::corrupt(Db::State, "snapshot", detail))
                }
            };
        self.lease(current, &serving)
    }

    /// The current snapshot, leased.
    ///
    /// A store whose last publication failed cannot say what is current: the
    /// marker may have been replaced and not made durable, so the value held
    /// here and the value on disk may already disagree. That check, and the
    /// latch for a lease that cannot open the file it names, are the reason
    /// this goes through the publication critical section rather than taking
    /// the mutex itself.
    pub(crate) fn current_snapshot(
        &self,
    ) -> StorageResult<Option<Snapshot<CoordinationTypeConfig>>> {
        self.under_current(|this, current| match current.clone() {
            Some(file) => this.lease(current, &file).map(Some),
            None => Ok(None),
        })
    }

    /// Pin a snapshot file and describe it as OpenRaft's `Snapshot`.
    ///
    /// Takes what only the guard can produce, rather than merely documenting
    /// that requirement. Choosing a file and pinning it have to be one step:
    /// [`publish`](Self::publish) retires and unlinks a superseded body under
    /// this same lock, so a caller that selected a file, released the lock, and
    /// then leased it could be handed a path that no longer exists. A borrow of
    /// the guarded value is unreachable without holding the mutex, so the
    /// parameter is unused at runtime and load-bearing at compile time.
    #[allow(clippy::result_large_err)]
    fn lease(
        &self,
        _current: &Option<SnapshotFile>,
        file: &SnapshotFile,
    ) -> StorageResult<Snapshot<CoordinationTypeConfig>> {
        Ok(Snapshot {
            meta: rows::snapshot_meta_from_wire(&file.meta)?,
            snapshot: Box::new(self.catalog.lease(&file.meta.snapshot_id, &file.path)?),
        })
    }
}

/// The snapshot builder.
///
/// Carries its own copies rather than borrowing the store: OpenRaft takes the
/// builder away and may call it while the state machine is being used
/// elsewhere, which is the whole reason `get_snapshot_builder` returns a value
/// instead of a reference.
#[derive(Clone, Debug)]
pub(crate) struct CoordinationSnapshotBuilder {
    machine: CoordinationStateMachine,
}

impl RaftSnapshotBuilder<CoordinationTypeConfig> for CoordinationSnapshotBuilder {
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<CoordinationTypeConfig>, RaftStorageError<CoordinationNodeId>> {
        let read = self
            .machine
            .handle
            .read_transaction()
            .await
            .map_err(io_read_state)?;

        // Off the actor thread and off the reactor: streaming a body is
        // blocking file I/O proportional to the state, and the read transaction
        // pins its own version, so neither the writer nor the runtime waits on
        // it.
        //
        // The closure hands the lease back with the body. Dropping the whole
        // `GuardedRead` here would end the lease at the same moment as the
        // transaction, leaving publication -- which happens next, and writes
        // into this store's directory -- outside the window the drain waits
        // for.
        let layout = self.machine.layout.clone();
        let (lease, built) = tokio::task::spawn_blocking(move || {
            let built = snapshot::build(&layout, &read);
            (read.release_transaction(), built)
        })
        .await
        .map_err(|err| {
            io_state(StorageError::corrupt(
                Db::State,
                "snapshot",
                format!("the snapshot builder did not finish: {err}"),
            ))
        })?;
        let built = built.map_err(io_state)?;

        self.machine.publish(&lease, built).map_err(io_state)
    }
}

/// Install lands in Checkpoint D's second half, and refusing is the honest
/// placeholder.
///
/// Not `unimplemented!()`: a panic inside a library is not a failure a runtime
/// can report, so a caller that has got ahead of the plan should get an error
/// saying so rather than take the process down.
fn not_yet_implemented(what: &'static str) -> RaftStorageError<CoordinationNodeId> {
    io_state(StorageError::corrupt(
        Db::State,
        "snapshot",
        format!("{what} is not implemented yet"),
    ))
}

impl RaftStateMachine<CoordinationTypeConfig> for CoordinationStateMachine {
    type SnapshotBuilder = CoordinationSnapshotBuilder;

    async fn applied_state(&mut self) -> Result<Applied, RaftStorageError<CoordinationNodeId>> {
        self.handle
            .call(StateCommand::AppliedState)
            .await
            .map_err(io_read_state)
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<CommandReplyV1>, RaftStorageError<CoordinationNodeId>>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        let entries: Vec<Entry> = entries.into_iter().collect();
        self.handle
            .call(|reply| StateCommand::Apply { entries, reply })
            .await
            .map_err(io_state)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        CoordinationSnapshotBuilder {
            machine: self.clone(),
        }
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<CoordinationSnapshotFile>, RaftStorageError<CoordinationNodeId>> {
        super::snapshot::begin_receiving(&self.layout)
            .map(Box::new)
            .map_err(io_state)
    }

    async fn install_snapshot(
        &mut self,
        _meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>,
        _snapshot: Box<CoordinationSnapshotFile>,
    ) -> Result<(), RaftStorageError<CoordinationNodeId>> {
        Err(not_yet_implemented("install_snapshot"))
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<CoordinationTypeConfig>>, RaftStorageError<CoordinationNodeId>>
    {
        self.current_snapshot().map_err(io_read_state)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordination::contract::{
        AcquireActionV1, ActionV1, ClientCommandV1, CommandV1, CompareAndSetActionV1,
        ExpireCommandV1, MembershipProgressCommandV1, ReleaseWithValueActionV1, RenewActionV1,
        ResourceKeyV1, SetGrantActionV1,
    };
    use crate::coordination::model::limits::ALL_RIGHTS;
    use crate::coordination::model::state::{GroupAuthority, ResourceKey, State};
    use crate::coordination::reason::Outcome as ModelOutcome;
    use openraft::{CommittedLeaderId, Membership};
    use redb::ReadableDatabase as _;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    const ACTOR: [u8; 32] = [0x0A; 32];
    const CLIENT: [u8; 16] = [0x0C; 16];
    const LEADER: [u8; 32] = [0x11; 32];

    fn scratch_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "aster-coordination-machine-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        dir
    }

    fn resource() -> ResourceKey {
        ResourceKey::new("ns", b"lock".to_vec()).expect("valid")
    }

    /// A group that is already running: one administrator, and `ACTOR` granted
    /// every right on one resource, so an ordinary command can succeed rather
    /// than being refused for authorization before it reaches storage.
    fn genesis_state() -> DurableState {
        let mut model = State {
            authority: crate::coordination::model::authority::testing::authority(),
            deployment_profile_code: crate::coordination::model::shape::PROFILE_PROD,
            admin_revision: 1,
            ..State::default()
        };
        model.admins.insert(ACTOR);
        // The resource row goes in with the grant. §8.1 moves the two together,
        // so a grant without one is a state apply cannot produce -- and the
        // validator now says so, which is how this fixture was found to be
        // describing an impossible group.
        model.resources.insert(
            resource(),
            crate::coordination::model::state::ResourceRow::default(),
        );
        model.acl.insert((resource(), ACTOR), ALL_RIGHTS);
        model.growth_bytes = model.recompute_growth();
        model.reserve_bytes = model.recompute_reserve();
        DurableState {
            model,
            membership: StoredMembership::default(),
            recovery_export_hash: None,
            recovered_from: None,
        }
    }

    /// A bound `state.redb` holding `genesis_state`, ready for an actor.
    fn seeded(dir: &std::path::Path) -> Database {
        seeded_with(dir, genesis_state())
    }

    /// The same seed under a caller-chosen state, for the tests that need two
    /// stores that are *not* the same group.
    fn seeded_with(dir: &std::path::Path, state: DurableState) -> Database {
        let database = Database::create(dir.join("state.redb")).expect("create");
        let write = super::super::schema::durable_write(&database, Db::State).expect("write");
        super::super::schema::create_state_tables(&write).expect("tables");
        write.commit().expect("commit");
        // The §11.1 directories a real open would have made. A snapshot build
        // writes into `snapshots/`, and a fixture that omitted it would fail
        // for the wrong reason.
        std::fs::create_dir_all(dir.join("snapshots")).expect("snapshots dir");
        std::fs::create_dir_all(dir.join("incoming")).expect("incoming dir");

        // Every key the state holds, in every section. A change set that named
        // only three of the seven wrote three of the seven and left the
        // counters describing a state the rows do not add up to -- which
        // `validate` then reports, confusingly, as corruption.
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
        changes
            .sessions
            .extend(state.model.sessions.keys().cloned());
        changes.intents.extend(state.model.intents.keys().copied());
        changes.routes.extend(state.model.routes.keys().copied());
        changes.audit.extend(state.model.audit.keys().copied());
        rows::commit(&database, state.clone(), &changes).expect("seed");
        // Written with the codec version the loader checks, exactly as
        // `schema::open` would.
        let write = super::super::schema::durable_write(&database, Db::State).expect("write");
        {
            let mut table = write
                .open_table(super::super::schema::STATE_META)
                .expect("meta");
            table
                .insert(
                    super::super::schema::KEY_CODEC_VERSION,
                    super::super::identity::STORAGE_CODEC_VERSION
                        .to_be_bytes()
                        .as_slice(),
                )
                .expect("insert");
        }
        write.commit().expect("commit");
        database
    }

    /// The drain rides along: dropping it is what waits out an open read, and
    /// a test that dropped it early would be asserting against a store whose
    /// shutdown ordering it had already broken.
    type Store = (CoordinationStateMachine, StateShutdown, ReadDrain);

    fn machine(dir: &std::path::Path) -> Store {
        CoordinationStateMachine::new(seeded(dir), StorageLayout::new(dir))
            .expect("open the state machine")
    }

    /// Reopen the same directory, which is what proves a write was durable.
    fn reopen(dir: &std::path::Path) -> Store {
        let database = Database::create(dir.join("state.redb")).expect("reopen");
        CoordinationStateMachine::new(database, StorageLayout::new(dir))
            .expect("reopen the state machine")
    }

    /// The durable state a marker check compares against.
    fn durable(dir: &std::path::Path) -> DurableState {
        rows::load(&opened(dir)).expect("load")
    }

    fn opened(dir: &std::path::Path) -> Database {
        Database::create(dir.join("state.redb")).expect("open")
    }

    /// Read `CURRENT` back from disk, through one database handle.
    ///
    /// redb refuses a second open of the same file, so the state and the marker
    /// check have to share one -- which is also how the production path does it.
    fn reload_current(
        layout: &StorageLayout,
        dir: &std::path::Path,
    ) -> super::super::error::StorageResult<Option<super::snapshot::SnapshotFile>> {
        let database = opened(dir);
        let state = rows::load(&database).expect("load");
        super::snapshot::load_current(layout, &database, &state)
    }

    fn log_id(index: u64) -> RaftLogId<CoordinationNodeId> {
        RaftLogId {
            leader_id: CommittedLeaderId::new(1, CoordinationNodeId::from_array(LEADER)),
            index,
        }
    }

    fn blank(index: u64) -> Entry {
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Blank,
        }
    }

    fn membership_entry(index: u64, voters: &[[u8; 32]]) -> Entry {
        let ids: std::collections::BTreeSet<CoordinationNodeId> = voters
            .iter()
            .map(|v| CoordinationNodeId::from_array(*v))
            .collect();
        let nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            ids.iter().map(|id| (*id, CoordinationNode)).collect();
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Membership(Membership::new(vec![ids], nodes)),
        }
    }

    fn normal(index: u64, command: CommandV1) -> Entry {
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Normal(command),
        }
    }

    /// A client command with the **canonical** request hash for its action.
    ///
    /// Not an arbitrary 32 bytes: §10 narrows a committed command only if its
    /// hash matches the action it carries, so a fixture that invents one
    /// produces a committed shape rejection rather than the command it meant to
    /// write -- which is what the first version of these tests did.
    fn client(sequence: u64, action: ActionV1) -> CommandV1 {
        let hash = crate::coordination::hash::request_hash(&action).expect("hashable");
        CommandV1::Client(ClientCommandV1 {
            actor_node_id: ACTOR.to_vec(),
            client_id: CLIENT.to_vec(),
            sequence,
            request_hash: hash.to_vec(),
            action,
        })
    }

    fn acquire_action() -> ActionV1 {
        ActionV1::Acquire(AcquireActionV1 {
            resource: ResourceKeyV1 {
                namespace: "ns".into(),
                key: b"lock".to_vec(),
            },
            ttl_ms: 30_000,
        })
    }

    fn acquire(sequence: u64) -> CommandV1 {
        client(sequence, acquire_action())
    }

    async fn published(machine: &CoordinationStateMachine) -> DurableState {
        *machine.handle().published().await.expect("published")
    }

    // ── Native entries ──────────────────────────────────────────────────────

    /// A blank advances `last_applied` and writes nothing else: no audit row,
    /// no transition, and one `NonCommand` reply.
    #[tokio::test]
    async fn a_blank_advances_last_applied_and_nothing_else() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);
        let before = published(&machine).await;

        let replies = machine.apply([blank(1)]).await.expect("apply");
        assert_eq!(replies.len(), 1, "one reply per applied entry");
        assert!(matches!(replies[0], CommandReplyV1::NonCommand(_)));

        let after = published(&machine).await;
        assert_eq!(after.model.last_applied.map(|id| id.index), Some(1));
        assert!(after.model.audit.is_empty(), "a blank audits nothing");
        assert_eq!(
            after.model.resources, before.model.resources,
            "and runs no transition"
        );
        assert_eq!(after.model.growth_bytes, before.model.growth_bytes);
        assert_eq!(after.model.reserve_bytes, before.model.reserve_bytes);

        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let (applied, _) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied.map(|id| id.index), Some(1), "and it was durable");
    }

    /// A membership entry records the exact `StoredMembership`, and it survives
    /// a reopen -- which is what `applied_state` must return at startup.
    #[tokio::test]
    async fn a_membership_entry_records_the_stored_membership_durably() {
        let dir = scratch_dir();
        let voters = [[1u8; 32], [2u8; 32], [3u8; 32]];
        let (mut machine, shutdown, _drain) = machine(&dir);

        let replies = machine
            .apply([membership_entry(4, &voters)])
            .await
            .expect("apply");
        assert!(matches!(replies[0], CommandReplyV1::NonCommand(_)));

        let after = published(&machine).await;
        assert!(
            after.model.audit.is_empty(),
            "a membership entry audits nothing"
        );
        assert_eq!(
            after.model.membership.voters,
            voters
                .iter()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            "the model's member set is derived from the entry"
        );
        assert_eq!(after.model.membership_log_id.map(|id| id.index), Some(4));

        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let (applied, membership) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied.map(|id| id.index), Some(4));
        assert_eq!(membership.log_id().map(|id| id.index), Some(4));
        assert_eq!(membership.membership().voter_ids().count(), 3);
    }

    // ── Delivery ────────────────────────────────────────────────────────────

    /// An index above `last_applied + 1` is accepted. OpenRaft's own suite
    /// delivers index 2 with nothing applied, and §10.2's guard requires
    /// strictly greater rather than contiguous -- append continuity is the log
    /// store's rule, not this one's.
    #[tokio::test]
    async fn an_advancing_but_non_contiguous_index_is_accepted() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        machine.apply([blank(1)]).await.expect("index 1");
        machine.apply([blank(9)]).await.expect("index 9 after 1");

        let after = published(&machine).await;
        assert_eq!(after.model.last_applied.map(|id| id.index), Some(9));
    }

    /// A delivery at or below `last_applied` is fatal before any mutation, and
    /// the store stays fatal afterwards. It is not an idempotent apply path.
    #[tokio::test]
    async fn a_delivery_at_or_below_last_applied_is_fatal() {
        for repeat in [5u64, 4] {
            let dir = scratch_dir();
            let (mut machine, _shutdown, _drain) = machine(&dir);
            machine.apply([blank(5)]).await.expect("index 5");

            let err = machine
                .apply([blank(repeat)])
                .await
                .expect_err("a redelivered index");
            assert!(
                format!("{err:?}").contains("LogIdNotAdvancing"),
                "index {repeat}: {err:?}"
            );
            assert!(
                machine.handle().is_fatal(),
                "index {repeat}: the store must latch fatal"
            );
            assert!(
                machine.apply([blank(6)]).await.is_err(),
                "index {repeat}: and stay fatal"
            );
        }
    }

    /// A command this build cannot interpret is fatal on every voter (§5.3.1),
    /// not skipped -- skipping would diverge from the voters that can apply it.
    #[tokio::test]
    async fn an_unknown_command_is_fatal() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let err = machine
            .apply([normal(
                1,
                CommandV1::Unknown(fory_core::UnknownCase::new(99u32, ())),
            )])
            .await
            .expect_err("an unknown variant");
        assert!(format!("{err:?}").contains("UnknownCommand"), "{err:?}");
        assert!(machine.handle().is_fatal());

        // And nothing was applied: `last_applied` must not record an entry this
        // voter could not interpret.
        drop(_shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let (applied, _) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied, None);
    }

    // ── Normal commands ─────────────────────────────────────────────────────

    /// A committed command persists its effects, its audit row, its session and
    /// both counters, and every one of them survives a reopen.
    #[tokio::test]
    async fn a_committed_command_persists_everything_it_wrote() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let replies = machine.apply([normal(1, acquire(1))]).await.expect("apply");
        assert_eq!(replies.len(), 1);
        let CommandReplyV1::Client(reply) = &replies[0] else {
            panic!(
                "a client command answers a client reply, got {:?}",
                replies[0]
            );
        };
        assert_eq!(reply.outcome_code, ModelOutcome::Granted.code());

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let state = published(&machine).await;

        let row = state
            .model
            .resources
            .get(&resource())
            .expect("the lease row");
        assert!(row.held && row.holder == Some(ACTOR));
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(1));
        assert_eq!(state.model.audit.len(), 1, "exactly one row per command");
        assert!(state.model.sessions.contains_key(&(ACTOR, CLIENT)));
        assert!(state.model.growth_bytes > 0 && state.model.reserve_bytes > 0);
        // The load revalidates, so getting here at all means the counters and
        // the audit tip recomputed from the rows on disk.
    }

    /// A duplicate request committed at a new, higher index replays its saved
    /// reply verbatim, writes its own audit row, and repeats no effect.
    #[tokio::test]
    async fn a_committed_replay_answers_from_the_session_and_still_audits() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let first = machine.apply([normal(1, acquire(1))]).await.expect("first");
        let before = published(&machine).await;

        let second = machine
            .apply([normal(2, acquire(1))])
            .await
            .expect("replay");
        // §4.3: a replay differs from the first attempt in exactly one field.
        // The value was never saved, so it is reported redacted rather than
        // absent -- and every other field comes from the saved snapshot, which
        // is what makes the replay exact.
        let (CommandReplyV1::Client(a), CommandReplyV1::Client(b)) = (&first[0], &second[0]) else {
            panic!("expected client replies");
        };
        let mut redacted = a.clone();
        let snapshot = redacted.snapshot.as_mut().expect("a snapshot");
        snapshot.value = None;
        snapshot.value_redacted = true;
        assert_eq!(&redacted, b, "only the value and its flag may differ");

        let after = published(&machine).await;
        assert_eq!(
            after.model.resources, before.model.resources,
            "a replay repeats no business effect"
        );
        assert_eq!(
            after.model.audit.len(),
            2,
            "but it is still an applied entry"
        );
        assert_eq!(after.model.last_applied.map(|id| id.index), Some(2));
    }

    /// A committed rejection is saved, audited, and replays identically after a
    /// reopen -- the property a client's retry depends on.
    #[tokio::test]
    async fn a_committed_rejection_survives_a_reopen_and_replays() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        // Sequence 2 with nothing at sequence 1 is refused by the classifier.
        let rejected = machine.apply([normal(1, acquire(9))]).await.expect("apply");
        let CommandReplyV1::Client(reply) = &rejected[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(reply.outcome_code, ModelOutcome::Rejected.code());
        assert_ne!(
            reply.reason_code, 0,
            "a classifier refusal keeps its reason"
        );

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let state = published(&machine).await;
        assert_eq!(state.model.audit.len(), 1, "a rejection is still audited");
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(1));
    }

    /// A committed `CounterExhausted` survives a reopen and replays exactly.
    ///
    /// §5.2 makes reason `15` terminal, so the saved reply is the client's
    /// permanent answer -- and a saved answer that did not survive a restart
    /// would let the same sequence be reclassified into a different one.
    #[tokio::test]
    async fn a_committed_counter_exhaustion_survives_a_reopen_and_replays() {
        let dir = scratch_dir();

        // A lease whose epoch is already at §5.2's ceiling, so the next acquire
        // has nowhere to advance it to.
        let database = seeded(&dir);
        let mut state = genesis_state();
        state.model.resources.insert(
            resource(),
            crate::coordination::model::state::ResourceRow {
                epoch: crate::coordination::model::limits::MAX_COUNTER,
                lease_revision: crate::coordination::model::limits::MAX_COUNTER,
                ..Default::default()
            },
        );
        state.model.growth_bytes = state.model.recompute_growth();
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes.resources.insert(resource());
        rows::commit(&database, state.clone(), &changes).expect("seed the exhausted row");
        drop(database);

        let (mut machine, shutdown, _drain) = reopen(&dir);
        let replies = machine.apply([normal(1, acquire(1))]).await.expect("apply");
        let CommandReplyV1::Client(first) = &replies[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(first.outcome_code, ModelOutcome::Rejected.code());
        assert_eq!(
            first.reason_code,
            crate::coordination::reason::Reason::CounterExhausted.code(),
            "the exhaustion must be the reason, not a generic rejection"
        );
        let first = first.clone();

        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let state = published(&machine).await;
        assert_eq!(state.model.audit.len(), 1, "the rejection was audited");
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(1));

        // And the client's retry replays the same terminal answer rather than
        // being reclassified.
        let replayed = machine
            .apply([normal(2, acquire(1))])
            .await
            .expect("replay");
        let CommandReplyV1::Client(second) = &replayed[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(second.outcome_code, first.outcome_code);
        assert_eq!(second.reason_code, first.reason_code);
    }

    /// A **known** command with a malformed field is a committed rejection, not
    /// a halt: every voter refuses it identically, and halting would make a
    /// malformed field an availability kill switch (§10.1).
    #[tokio::test]
    async fn a_malformed_known_command_is_a_committed_rejection() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let malformed = client(
            1,
            ActionV1::SetGrant(SetGrantActionV1 {
                resource: ResourceKeyV1 {
                    namespace: "ns".into(),
                    key: b"lock".to_vec(),
                },
                // Not a node id. The hash is canonical, so the width is what
                // refuses this rather than a hash mismatch.
                target_node_id: vec![0u8; 8],
                rights: ALL_RIGHTS,
                expected_acl_revision: 0,
            }),
        );

        let replies = machine
            .apply([normal(1, malformed)])
            .await
            .expect("not fatal");
        let CommandReplyV1::Client(reply) = &replies[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(reply.outcome_code, ModelOutcome::Rejected.code());
        assert!(
            !machine.handle().is_fatal(),
            "a malformed field is not a halt"
        );

        let state = published(&machine).await;
        assert_eq!(
            state.model.audit.len(),
            1,
            "and it is audited like any other"
        );
    }

    /// A system command audits under the system code space, not as a client
    /// outcome, and answers a system reply.
    #[tokio::test]
    async fn a_system_command_audits_in_its_own_code_space() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        machine
            .apply([normal(1, acquire(1))])
            .await
            .expect("acquire");

        let held = published(&machine).await;
        let row = held.model.resources.get(&resource()).expect("the row");

        let replies = machine
            .apply([normal(
                2,
                CommandV1::Expire(ExpireCommandV1 {
                    resource: ResourceKeyV1 {
                        namespace: "ns".into(),
                        key: b"lock".to_vec(),
                    },
                    epoch: row.epoch,
                    lease_revision: row.lease_revision,
                }),
            )])
            .await
            .expect("expire");
        assert!(matches!(replies[0], CommandReplyV1::System(_)));

        let state = published(&machine).await;
        let audit = state.model.audit.get(&(1, 2)).expect("the expiry's row");
        assert!(
            !audit.result.is_client(),
            "a system result is not a client outcome"
        );
        assert!(!state.model.resources[&resource()].held, "and it expired");
    }

    /// A stale membership progress report commits harmlessly: it names an
    /// operation that does not exist, so it changes nothing and still audits.
    #[tokio::test]
    async fn a_stale_membership_report_commits_and_audits_without_effect() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let replies = machine
            .apply([normal(
                1,
                CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                    operation_id: vec![0x77; 16],
                    expected_phase: 1,
                    new_phase: 2,
                    observed_membership_log_id: None,
                    result_code: 0,
                }),
            )])
            .await
            .expect("apply");
        assert!(matches!(replies[0], CommandReplyV1::System(_)));

        let state = published(&machine).await;
        assert!(state.model.intents.is_empty(), "it named no real intent");
        assert_eq!(state.model.audit.len(), 1, "and is audited anyway");
    }

    // ── Success corridors ───────────────────────────────────────────────────
    //
    // The long generated history reaches every command shape but only ever
    // *refuses* four of them: `renew`, `compare_and_set`, `release_with_value`
    // and `cancel_membership` need a lease or an intent that its interleaved
    // steps have usually already taken away. Reshaping the alphabet to fix that
    // would move every history-derived assertion in the model, so the coverage
    // comes from short deliberate corridors instead -- each one applied through
    // the real state machine and then reopened, which is the half a model-level
    // test cannot give.

    fn fence(epoch: u64) -> crate::coordination::contract::FenceV1 {
        crate::coordination::contract::FenceV1 {
            group_id: crate::coordination::model::authority::testing::authority()
                .group_id
                .to_vec(),
            resource: ResourceKeyV1 {
                namespace: "ns".into(),
                key: b"lock".to_vec(),
            },
            epoch,
            holder: ACTOR.to_vec(),
        }
    }

    /// The whole lease lifecycle applies and survives a reopen.
    ///
    /// One corridor rather than four tests: the interesting part is that each
    /// step's fence and expected revision come from the state the previous step
    /// left, so a step that silently failed would strand the next one.
    #[tokio::test]
    async fn a_lease_corridor_applies_and_survives_a_reopen() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let granted = |replies: &[CommandReplyV1], step: &str| -> i32 {
            let CommandReplyV1::Client(reply) = &replies[0] else {
                panic!("{step}: expected a client reply, got {:?}", replies[0]);
            };
            assert_ne!(
                reply.outcome_code,
                ModelOutcome::Rejected.code(),
                "{step} was refused with reason {}",
                reply.reason_code
            );
            reply.outcome_code
        };

        // 1. acquire
        let replies = machine
            .apply([normal(1, acquire(1))])
            .await
            .expect("acquire");
        assert_eq!(granted(&replies, "acquire"), ModelOutcome::Granted.code());
        let epoch = published(&machine).await.model.resources[&resource()].epoch;

        // 2. renew, fenced by what the acquire established
        let replies = machine
            .apply([normal(
                2,
                client(
                    2,
                    ActionV1::Renew(RenewActionV1 {
                        fence: fence(epoch),
                    }),
                ),
            )])
            .await
            .expect("renew");
        assert_eq!(granted(&replies, "renew"), ModelOutcome::Renewed.code());

        // 3. compare-and-set at the current value revision
        let before = published(&machine).await;
        let row = &before.model.resources[&resource()];
        assert!(
            row.held && row.holder == Some(ACTOR),
            "still held after renew"
        );
        let replies = machine
            .apply([normal(
                3,
                client(
                    3,
                    ActionV1::CompareAndSet(CompareAndSetActionV1 {
                        fence: fence(row.epoch),
                        expected_value_revision: row.value_revision,
                        new_value: b"payload".to_vec(),
                    }),
                ),
            )])
            .await
            .expect("compare and set");
        assert_eq!(
            granted(&replies, "compare_and_set"),
            ModelOutcome::Mutated.code()
        );

        // 4. release with a final value
        let before = published(&machine).await;
        let row = &before.model.resources[&resource()];
        assert_eq!(row.value, b"payload", "the CAS value is what the row holds");
        let replies = machine
            .apply([normal(
                4,
                client(
                    4,
                    ActionV1::ReleaseWithValue(ReleaseWithValueActionV1 {
                        fence: fence(row.epoch),
                        expected_value_revision: row.value_revision,
                        final_value: b"final".to_vec(),
                    }),
                ),
            )])
            .await
            .expect("release with value");
        assert_eq!(
            granted(&replies, "release_with_value"),
            ModelOutcome::Released.code()
        );

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let state = published(&machine).await;
        let row = &state.model.resources[&resource()];
        assert!(!row.held && row.holder.is_none(), "the lease was released");
        assert_eq!(row.value, b"final", "and the final value is durable");
        assert_eq!(state.model.audit.len(), 4, "one row per committed command");
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(4));
    }

    /// A membership intent is accepted, then cancelled, and both survive a
    /// reopen.
    ///
    /// `cancel_membership` is the fourth operation the long history never
    /// reaches: cancelling needs an intent in a cancellable phase at the moment
    /// the command fires, and the generator's interleaving rarely leaves one.
    #[tokio::test]
    async fn a_membership_cancel_corridor_applies_and_survives_a_reopen() {
        use crate::coordination::contract::{
            CancelMembershipActionV1, ChangeMembershipActionV1, MemberSeedV1, NodeIdV1,
        };
        use crate::coordination::model::authority::testing as auth;
        use crate::coordination::model::membership::{test_ticket, PHASE_CANCELLED, PHASE_PENDING};

        let dir = scratch_dir();
        // The corridor needs a running group: §12.1 admits at most two new
        // voters per change, so a plan proposed against an empty membership is
        // refused before it can be cancelled.
        let database = seeded(&dir);
        let voters: Vec<[u8; 32]> = (1u8..=3).map(|b| [b; 32]).collect();
        let mut state = genesis_state();
        state.model.membership =
            crate::coordination::model::state::CommittedMembership::uniform(voters.clone());
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
        rows::commit(&database, state.clone(), &changes).expect("seed");
        drop(database);

        // The stored membership has to agree with the projection, so the store
        // is opened through a state whose `StoredMembership` really names them.
        let (mut machine, shutdown, _drain) = reopen(&dir);
        machine
            .apply([membership_entry(1, &voters)])
            .await
            .expect("the native membership entry");

        let operation = [0x42u8; 16];
        let seeds: Vec<MemberSeedV1> = voters
            .iter()
            .map(|node| MemberSeedV1 {
                node_id: node.to_vec(),
                ticket: String::from_utf8(test_ticket(node)).expect("a text ticket"),
                // Already members, so §9 wants no join permit.
                signed_join_permit: None,
            })
            .collect();
        let plan = ChangeMembershipActionV1 {
            operation_id: operation.to_vec(),
            expected_membership_log_id: Some(crate::coordination::contract::LogIdV1 {
                term: 1,
                leader_node_id: LEADER.to_vec(),
                index: 1,
            }),
            desired_voters: voters
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            retained_learners: Vec::new(),
            member_seeds: seeds,
        };
        let _ = auth::authority();

        let replies = machine
            .apply([normal(2, client(1, ActionV1::ChangeMembership(plan)))])
            .await
            .expect("accept");
        let CommandReplyV1::Client(reply) = &replies[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(
            reply.outcome_code,
            ModelOutcome::MembershipAccepted.code(),
            "the plan must be accepted, not refused with reason {}",
            reply.reason_code
        );
        assert_eq!(
            published(&machine).await.model.intents[&operation].status_code,
            PHASE_PENDING
        );

        let replies = machine
            .apply([normal(
                3,
                client(
                    2,
                    ActionV1::CancelMembership(CancelMembershipActionV1 {
                        operation_id: operation.to_vec(),
                        expected_phase_code: PHASE_PENDING,
                    }),
                ),
            )])
            .await
            .expect("cancel");
        let CommandReplyV1::Client(reply) = &replies[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(
            reply.outcome_code,
            ModelOutcome::MembershipCancelled.code(),
            "cancelling a pending intent must succeed, not fail with reason {}",
            reply.reason_code
        );

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let state = published(&machine).await;
        let intent = &state.model.intents[&operation];
        assert_eq!(intent.status_code, PHASE_CANCELLED, "and it is durable");
        assert!(
            intent.completed_log_id.is_some(),
            "a terminal intent carries the id it completed at"
        );
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(3));
    }

    // ── States a legal group reaches and a store must reopen ────────────────

    /// A membership carrying four learners reopens.
    ///
    /// §9 bounds retained and transient learners at two **each**, so a group in
    /// `AddingLearners` legally names four. A store that read the retained
    /// bound as the total would open cleanly right up until the moment a change
    /// added its transient learners, and then refuse to start.
    ///
    /// Driven through a native membership entry and a real reopen, because the
    /// failure is at load: the running store had already accepted the state.
    #[tokio::test]
    async fn a_membership_with_four_learners_reopens() {
        use openraft::Membership;

        use crate::coordination::model::membership::PHASE_ADDING_LEARNERS;
        use crate::coordination::model::state::{Intent, MembershipPlan};

        let dir = scratch_dir();
        // Four learners are legal *because* a change is in flight, so the group
        // has to actually be in one: an intent in `AddingLearners` is what
        // makes the two transient learners transient rather than retained.
        let database = seeded(&dir);
        let mut state = genesis_state();
        state.model.last_applied = Some(LogId::new(1, LEADER, 1));
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
        let operation = [0x61u8; 16];
        state.model.intents.insert(
            operation,
            Intent {
                actor: ACTOR,
                request_hash: [0x62u8; 32],
                expected_membership: None,
                plan: MembershipPlan {
                    desired_voters: (1u8..=3).map(|b| [b; 32]).collect(),
                    retained_learners: vec![[0x71u8; 32], [0x72u8; 32]],
                    member_seeds: Vec::new(),
                },
                status_code: PHASE_ADDING_LEARNERS,
                result_code: 0,
                accepted_log_id: LogId::new(1, LEADER, 1),
                completed_log_id: None,
                observed_membership_log_id: None,
            },
        );
        changes.intents.insert(operation);
        state.model.growth_bytes = state.model.recompute_growth();
        rows::commit(&database, state.clone(), &changes).expect("seed the change in flight");
        drop(database);

        let (mut machine, shutdown, _drain) = reopen(&dir);

        let voters: std::collections::BTreeSet<CoordinationNodeId> = (1u8..=3)
            .map(|b| CoordinationNodeId::from_array([b; 32]))
            .collect();
        // Two retained plus two transient, which is the widest §9 permits.
        let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            voters.iter().map(|id| (*id, CoordinationNode)).collect();
        for byte in [0x71u8, 0x72, 0x73, 0x74] {
            nodes.insert(CoordinationNodeId::from_array([byte; 32]), CoordinationNode);
        }
        machine
            .apply([Entry {
                log_id: log_id(2),
                payload: EntryPayload::Membership(Membership::new(vec![voters], nodes)),
            }])
            .await
            .expect("apply");

        let running = published(&machine).await;
        assert_eq!(running.model.membership.learners.len(), 4);
        assert_eq!(running.model.membership.voters.len(), 3);

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let restarted = published(&machine).await;
        assert_eq!(
            restarted.model.membership, running.model.membership,
            "four learners is a state a change reaches, so a store must reopen it"
        );
    }

    /// A joint membership keeps the voters it is moving out of.
    ///
    /// OpenRaft counts both configurations' members as voters while a change is
    /// in flight, and §9 protects a route for a node in the current committed
    /// membership. Projecting only the target configuration dropped the
    /// departing voters, so the store and OpenRaft disagreed about who could
    /// vote -- and a departing voter's route became evictable.
    #[tokio::test]
    async fn a_joint_membership_keeps_both_configurations_voters() {
        use openraft::Membership;

        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let ids = |bytes: &[u8]| -> std::collections::BTreeSet<CoordinationNodeId> {
            bytes
                .iter()
                .map(|b| CoordinationNodeId::from_array([*b; 32]))
                .collect()
        };
        let old = ids(&[1, 2, 3]);
        let new = ids(&[1, 2, 4]);
        let nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            old.union(&new).map(|id| (*id, CoordinationNode)).collect();

        machine
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(Membership::new(
                    vec![old.clone(), new.clone()],
                    nodes,
                )),
            }])
            .await
            .expect("apply");

        let running = published(&machine).await;
        assert!(running.model.membership.joint, "a change is in flight");
        assert_eq!(
            running.model.membership.voters,
            [1u8, 2, 3, 4]
                .iter()
                .map(|b| [*b; 32])
                .collect::<std::collections::BTreeSet<_>>(),
            "the departing voter still votes"
        );
        assert!(
            running.model.membership.contains(&[3u8; 32]),
            "and its route is still protected"
        );
        assert!(running.model.membership.learners.is_empty());

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        assert_eq!(
            published(&machine).await.model.membership,
            running.model.membership
        );
    }

    /// An intent that **fails** during `AddingLearners` leaves its transient
    /// learners behind, and the store reopens anyway.
    ///
    /// §9 lets any active phase transition to `Failed`, and `Failed` changes no
    /// membership -- so the four learners that were legal a moment earlier are
    /// still there with nothing in flight. A validator that required the
    /// retained two once no intent was active made this state unloadable: the
    /// running store had accepted it, and only a restart found out.
    ///
    /// Driven through the real transition rather than seeded, because the point
    /// is that apply produces it.
    #[tokio::test]
    async fn an_intent_that_fails_mid_change_leaves_a_reopenable_state() {
        use crate::coordination::contract::MembershipProgressCommandV1;
        use crate::coordination::model::membership::{
            PHASE_ADDING_LEARNERS, PHASE_FAILED, PHASE_PENDING,
        };
        use crate::coordination::model::state::{Intent, MembershipPlan};
        use openraft::Membership;

        let dir = scratch_dir();
        let database = seeded(&dir);
        let mut state = genesis_state();
        state.model.last_applied = Some(LogId::new(1, LEADER, 1));
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
        let operation = [0x64u8; 16];
        state.model.intents.insert(
            operation,
            Intent {
                actor: ACTOR,
                request_hash: [0x65u8; 32],
                expected_membership: None,
                plan: MembershipPlan {
                    desired_voters: (1u8..=3).map(|b| [b; 32]).collect(),
                    retained_learners: vec![[0x81u8; 32], [0x82u8; 32]],
                    member_seeds: Vec::new(),
                },
                status_code: PHASE_PENDING,
                result_code: 0,
                accepted_log_id: LogId::new(1, LEADER, 1),
                completed_log_id: None,
                observed_membership_log_id: None,
            },
        );
        changes.intents.insert(operation);
        state.model.growth_bytes = state.model.recompute_growth();
        rows::commit(&database, state.clone(), &changes).expect("seed a pending intent");
        drop(database);

        let (mut machine, shutdown, _drain) = reopen(&dir);

        // The controller adds every missing node as a learner, so the node map
        // grows past the retained two while the change is in flight.
        let voters: std::collections::BTreeSet<CoordinationNodeId> = (1u8..=3)
            .map(|b| CoordinationNodeId::from_array([b; 32]))
            .collect();
        let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            voters.iter().map(|id| (*id, CoordinationNode)).collect();
        for byte in [0x81u8, 0x82, 0x83, 0x84] {
            nodes.insert(CoordinationNodeId::from_array([byte; 32]), CoordinationNode);
        }
        machine
            .apply([Entry {
                log_id: log_id(2),
                payload: EntryPayload::Membership(Membership::new(vec![voters], nodes)),
            }])
            .await
            .expect("the learners land");

        let progress = |from: i32, to: i32| {
            CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                operation_id: operation.to_vec(),
                expected_phase: from,
                new_phase: to,
                observed_membership_log_id: None,
                result_code: crate::coordination::contract::SYS_MEMBERSHIP_ADVANCED,
            })
        };
        machine
            .apply([normal(3, progress(PHASE_PENDING, PHASE_ADDING_LEARNERS))])
            .await
            .expect("adding learners");
        // §9: any active phase may fail, and failing changes no membership.
        machine
            .apply([normal(4, progress(PHASE_ADDING_LEARNERS, PHASE_FAILED))])
            .await
            .expect("fail");

        let running = published(&machine).await;
        assert_eq!(
            running.model.intents[&operation].status_code, PHASE_FAILED,
            "the change failed"
        );
        assert_eq!(
            running.model.membership.learners.len(),
            4,
            "and left the learners it had introduced"
        );
        assert!(
            running.model.intents.values().all(|intent| {
                crate::coordination::model::membership::is_terminal_phase(intent.status_code)
            }),
            "with nothing in flight to justify them"
        );

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        assert_eq!(
            published(&machine).await.model.membership,
            running.model.membership,
            "a state apply produced must reopen, whatever the controller still owes"
        );
    }

    /// A membership entry over §9's envelope is refused at apply, not
    /// discovered at the next restart.
    ///
    /// Admission bounds the *plan*, so this can only arise from a controller
    /// that ignored it -- but the store is what persists the result, and a
    /// store that wrote it would apply cleanly and then fail to reopen. The
    /// worst place to find out is a restart, so the writer refuses what the
    /// reader would.
    #[tokio::test]
    async fn a_membership_entry_over_the_envelope_is_refused_before_it_is_written() {
        use crate::coordination::model::shape::MAX_LEARNER_ENVELOPE;
        use openraft::Membership;

        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let voters: std::collections::BTreeSet<CoordinationNodeId> = (1u8..=3)
            .map(|b| CoordinationNodeId::from_array([b; 32]))
            .collect();
        let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            voters.iter().map(|id| (*id, CoordinationNode)).collect();
        for n in 0..=MAX_LEARNER_ENVELOPE {
            nodes.insert(
                CoordinationNodeId::from_array([0x90 + n as u8; 32]),
                CoordinationNode,
            );
        }

        let err = machine
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(Membership::new(vec![voters], nodes)),
            }])
            .await
            .expect_err("one learner over the envelope");
        assert!(format!("{err:?}").contains("envelope"), "{err:?}");
        assert!(machine.handle().is_fatal());

        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let (applied, membership) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied, None, "nothing was written");
        assert!(membership.membership().nodes().next().is_none());
    }

    /// The §7.3 shrink that `retain = true` would have made unloadable.
    ///
    /// Five voters and six stranded learners, cleaning up to three voters. The
    /// plan introduces no node, so admission's projection is six and it is
    /// accepted -- correctly. What the controller then commits depends entirely
    /// on a flag §7.3 did not specify:
    ///
    /// * `retain = false` removes the two departing voters, leaving the six
    ///   learners that were already there. Within the envelope, and it loads.
    /// * `retain = true` turns them into learners, giving eight. Over the
    ///   envelope, so the store refuses it -- on every voter at once, because
    ///   every voter applies the same committed entry.
    ///
    /// Both memberships are built here rather than driven through a controller,
    /// because the controller is Slice 3's. What this pins is that the choice
    /// matters and which way it goes.
    #[tokio::test]
    async fn a_shrink_commits_only_under_the_normative_retain_mode() {
        use crate::coordination::model::shape::{MAX_LEARNER_ENVELOPE, MEMBERSHIP_RETAIN};
        use openraft::Membership;

        // No assertion on the constant itself -- it is a compile-time value, and
        // asserting a literal against itself proves nothing. The first half of
        // this test *uses* it, so flipping it to true makes that half fail with
        // the eight-learner refusal the second half describes. That is the
        // stronger statement anyway: the constant is load-bearing, not
        // documented.

        let node = |b: u8| CoordinationNodeId::from_array([b; 32]);
        let stranded: Vec<CoordinationNodeId> = (0..MAX_LEARNER_ENVELOPE)
            .map(|n| node(0x80 + n as u8))
            .collect();
        let departing = [node(4), node(5)];

        // What each flag commits, given voters {1..5} shrinking to {1,2,3}.
        let membership = |retain: bool| {
            let voters: std::collections::BTreeSet<CoordinationNodeId> =
                (1u8..=3).map(node).collect();
            let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
                voters.iter().map(|id| (*id, CoordinationNode)).collect();
            for learner in &stranded {
                nodes.insert(*learner, CoordinationNode);
            }
            if retain {
                for leaving in departing {
                    nodes.insert(leaving, CoordinationNode);
                }
            }
            Membership::new(vec![voters], nodes)
        };

        // The normative mode: the learners already there, and nothing added.
        let dir = scratch_dir();
        let (mut removing, shutdown, _drain) = machine(&dir);
        removing
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(membership(MEMBERSHIP_RETAIN)),
            }])
            .await
            .expect("a shrink under retain = false commits");
        let running = published(&removing).await;
        assert_eq!(
            running.model.membership.learners.len(),
            MAX_LEARNER_ENVELOPE
        );
        assert_eq!(running.model.membership.voters.len(), 3);

        drop(shutdown);
        drop(removing);
        let (restarted, restarted_shutdown, _drain) = reopen(&dir);
        assert_eq!(
            published(&restarted).await.model.membership,
            running.model.membership,
            "and it reopens"
        );
        drop(restarted_shutdown);
        drop(restarted);

        // The mode §7.3 now forbids, and what it would have cost.
        let dir = scratch_dir();
        let (mut retaining, _shutdown, _drain) = machine(&dir);
        let err = retaining
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(membership(true)),
            }])
            .await
            .expect_err("retain = true demotes the departing voters");
        assert!(format!("{err:?}").contains("envelope"), "{err:?}");
        assert!(
            retaining.handle().is_fatal(),
            "and it would have been fatal on every voter, not one"
        );
    }

    /// Step 5 must replace the node map, not add before it removes.
    ///
    /// `retain = false` removes a departing voter *even when the plan names it
    /// in `retained_learners`*, so step 4 can drop a node step 5 has to put
    /// back. That makes the ordering load-bearing: five voters with six
    /// stranded learners, shrinking to three while retaining two of the
    /// departing ones, is within the envelope at both ends and over it in
    /// between.
    ///
    /// The controller is Slice 3's, so what this pins is the shape of the
    /// entries it may commit. The atomic replacement applies and reopens; the
    /// re-add-first intermediate is refused on every voter.
    #[tokio::test]
    async fn step_five_replaces_the_node_map_rather_than_adding_first() {
        use crate::coordination::model::shape::MAX_LEARNER_ENVELOPE;
        use openraft::Membership;

        let node = |b: u8| CoordinationNodeId::from_array([b; 32]);
        let stranded: Vec<CoordinationNodeId> = (0..MAX_LEARNER_ENVELOPE)
            .map(|n| node(0x80 + n as u8))
            .collect();
        // The two departing voters the plan asks to keep as learners.
        let retained = [node(4), node(5)];
        let desired: std::collections::BTreeSet<CoordinationNodeId> = (1u8..=3).map(node).collect();

        let map = |learners: &[CoordinationNodeId]| {
            let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
                desired.iter().map(|id| (*id, CoordinationNode)).collect();
            for learner in learners {
                nodes.insert(*learner, CoordinationNode);
            }
            Membership::new(vec![desired.clone()], nodes)
        };

        // What §7.3 step 5 now requires: the exact voters plus the retained
        // learners, in one entry.
        let dir = scratch_dir();
        let (mut atomic, shutdown, _drain) = machine(&dir);
        atomic
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(map(&retained)),
            }])
            .await
            .expect("a node-map replacement commits");
        let running = published(&atomic).await;
        assert_eq!(running.model.membership.learners.len(), 2);

        drop(shutdown);
        drop(atomic);
        let (restarted, restarted_shutdown, _drain) = reopen(&dir);
        assert_eq!(
            published(&restarted).await.model.membership,
            running.model.membership,
            "and it reopens"
        );
        drop(restarted_shutdown);
        drop(restarted);

        // The intermediate a re-add-first sequence would commit: the six
        // stranded learners plus the two being restored.
        let mut both: Vec<CoordinationNodeId> = stranded.clone();
        both.extend(retained);
        assert_eq!(both.len(), MAX_LEARNER_ENVELOPE + 2);

        let dir = scratch_dir();
        let (mut adding, _shutdown, _drain) = machine(&dir);
        let err = adding
            .apply([Entry {
                log_id: log_id(1),
                payload: EntryPayload::Membership(map(&both)),
            }])
            .await
            .expect_err("adding before removing exceeds the envelope");
        assert!(format!("{err:?}").contains("envelope"), "{err:?}");
        assert!(
            adding.handle().is_fatal(),
            "and it would have been fatal on every voter"
        );
    }

    /// A plan admitted against one membership is fenced by
    /// `13 MembershipRevisionMismatch` when it applies against another --
    /// across a restart, through the real state machine.
    ///
    /// This ties together the four facts that make `25 MembershipLearnerEnvelope`
    /// unable to lose a race, none of which is guarded on its own:
    ///
    /// 1. `decide_change` compares the expected membership log id **before** it
    ///    reaches the envelope;
    /// 2. only a native `EntryPayload::Membership` changes the committed
    ///    membership;
    /// 3. that same entry advances `membership_log_id`;
    /// 4. a reopen derives both from the stored `StoredMembership`, so the
    ///    pairing survives a restart rather than being an in-memory
    ///    coincidence.
    ///
    /// The model-level tests assign `membership` and `membership_log_id` by
    /// hand, which is exactly how an earlier fixture came to describe a state no
    /// transition produces -- membership moved, log id did not -- and "proved" a
    /// race that cannot happen. Here nothing is assigned: both facts move only
    /// because a membership entry was applied, and the store is closed and
    /// reopened in between so the second half reads them off disk.
    #[tokio::test]
    async fn a_plan_admitted_against_one_membership_is_fenced_after_a_restart() {
        use crate::coordination::contract::{
            ChangeMembershipActionV1, LogIdV1, MemberSeedV1, NodeIdV1,
        };
        use crate::coordination::model::apply::{preflight, Admission, ClientRef};
        use crate::coordination::model::authority::testing as auth;
        use crate::coordination::model::boundary::action_from_wire;
        use crate::coordination::model::membership::test_ticket;
        use crate::coordination::model::state::CommittedMembership;
        use crate::coordination::reason::Reason;
        use openraft::Membership;

        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let voters: Vec<[u8; 32]> = (1u8..=3).map(|b| [b; 32]).collect();
        let native = |index: u64, learners: std::ops::Range<u8>| {
            let ids: std::collections::BTreeSet<CoordinationNodeId> = voters
                .iter()
                .map(|v| CoordinationNodeId::from_array(*v))
                .collect();
            let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
                ids.iter().map(|id| (*id, CoordinationNode)).collect();
            for byte in learners {
                nodes.insert(CoordinationNodeId::from_array([byte; 32]), CoordinationNode);
            }
            Entry {
                log_id: log_id(index),
                payload: EntryPayload::Membership(Membership::new(vec![ids], nodes)),
            }
        };

        // 1. L1: the retained two.
        machine.apply([native(1, 0x81..0x83)]).await.expect("L1");
        let at_l1 = published(&machine).await;
        assert_eq!(at_l1.model.membership.learners.len(), 2);
        let l1 = at_l1.model.membership_log_id.expect("L1 has a log id");
        assert_eq!(l1.index, 1, "the native entry is what set it");

        // 2. A plan against L1: two new voters and two new retained learners,
        //    so it introduces four and projects to exactly the envelope.
        let new_voters: Vec<[u8; 32]> = (0x91u8..0x93).map(|b| [b; 32]).collect();
        let new_learners: Vec<[u8; 32]> = (0x94u8..0x96).map(|b| [b; 32]).collect();
        let mut desired = voters.clone();
        desired.extend(new_voters.iter().copied());
        desired.sort_unstable();

        let seed = |node: &[u8; 32], state: &CommittedMembership| MemberSeedV1 {
            node_id: node.to_vec(),
            ticket: String::from_utf8(test_ticket(node)).expect("a text ticket"),
            signed_join_permit: (!state.contains(node))
                .then(|| auth::valid(node, &test_ticket(node))),
        };
        let mut member_seeds: Vec<MemberSeedV1> = desired
            .iter()
            .chain(&new_learners)
            .map(|node| seed(node, &at_l1.model.membership))
            .collect();
        member_seeds.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let plan = ActionV1::ChangeMembership(ChangeMembershipActionV1 {
            operation_id: vec![0x66; 16],
            // Bound to L1, which is the whole point: this is what the fence
            // compares against once the membership has moved.
            expected_membership_log_id: Some(LogIdV1 {
                term: l1.term,
                leader_node_id: l1.leader_node_id.to_vec(),
                index: l1.index,
            }),
            desired_voters: desired
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            retained_learners: new_learners
                .iter()
                .map(|n| NodeIdV1 {
                    node_id: n.to_vec(),
                })
                .collect(),
            member_seeds,
        });

        let requester = ClientRef {
            actor: ACTOR,
            client_id: CLIENT,
            sequence: 1,
            request_hash: crate::coordination::hash::request_hash(&plan).expect("hashable"),
        };
        assert_eq!(
            preflight(
                &at_l1.model,
                &requester,
                &action_from_wire(&plan).expect("narrows")
            ),
            Admission::Admit,
            "against L1 the plan fits the envelope exactly, so a leader proposes it"
        );

        // 3. L2 lands first: four more learners, taking the group to the full
        //    envelope. The log id moves with it, because the same entry sets
        //    both.
        machine.apply([native(2, 0x81..0x87)]).await.expect("L2");
        let at_l2 = published(&machine).await;
        assert_eq!(at_l2.model.membership.learners.len(), 6);
        assert_ne!(
            at_l2.model.membership_log_id,
            Some(l1),
            "the membership moved"
        );

        // 4. Close and reopen, so the second half reads both facts off disk
        //    rather than out of the actor's memory.
        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let restarted = published(&machine).await;
        assert_eq!(
            restarted.model.membership, at_l2.model.membership,
            "the derivation survives the restart"
        );
        assert_eq!(
            restarted.model.membership_log_id,
            at_l2.model.membership_log_id
        );

        // 5. The plan the leader proposed against L1, applied against L2.
        let replies = machine
            .apply([normal(3, client(1, plan))])
            .await
            .expect("committed, so it applies");
        let CommandReplyV1::Client(reply) = &replies[0] else {
            panic!("expected a client reply");
        };
        assert_eq!(reply.disposition_code, 1, "Consumed");
        assert_eq!(
            reply.reason_code,
            Reason::MembershipRevisionMismatch.code(),
            "the membership moved, and that is the fence"
        );
        assert_ne!(
            reply.reason_code,
            Reason::MembershipLearnerEnvelope.code(),
            "the envelope is unreachable here -- reaching it would mean the revision \
             check, the native-entry authority or the durable derivation had come apart"
        );
    }

    /// Sixty-four terminal intents *and* one active reopens.
    ///
    /// §9: "if all 64 slots hold terminal intents and one is active, a new
    /// intent is still accepted; the cap only bounds terminal history". A
    /// 64-row storage ceiling refused a state the transition is required to
    /// produce, and the only way to find out was to fill the table and restart.
    #[tokio::test]
    async fn sixty_four_terminal_intents_and_one_active_reopens() {
        use crate::coordination::model::membership::{PHASE_COMPLETED, PHASE_PENDING};
        use crate::coordination::model::state::{Intent, MembershipPlan};

        let dir = scratch_dir();
        let database = seeded(&dir);
        let mut state = genesis_state();

        let voters: Vec<[u8; 32]> = (1u8..=3).map(|b| [b; 32]).collect();
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
        state.model.last_applied = Some(LogId::new(1, LEADER, 1_000));

        let intent = |phase: i32, n: u64| Intent {
            actor: ACTOR,
            request_hash: [0x31u8; 32],
            expected_membership: None,
            plan: MembershipPlan {
                desired_voters: voters.clone(),
                retained_learners: Vec::new(),
                member_seeds: Vec::new(),
            },
            status_code: phase,
            result_code: 0,
            accepted_log_id: LogId::new(1, LEADER, n),
            completed_log_id: (phase == PHASE_COMPLETED).then(|| LogId::new(1, LEADER, n + 1)),
            observed_membership_log_id: None,
        };

        for n in 0..crate::coordination::model::limits::MAX_RETAINED_INTENTS {
            let mut id = [0u8; 16];
            id[0..8].copy_from_slice(&n.to_be_bytes());
            state
                .model
                .intents
                .insert(id, intent(PHASE_COMPLETED, n + 1));
            changes.intents.insert(id);
        }
        // The one active intent §9 never evicts, on top of a full terminal
        // history.
        let active = [0xFFu8; 16];
        state
            .model
            .intents
            .insert(active, intent(PHASE_PENDING, 900));
        changes.intents.insert(active);
        state.model.growth_bytes = state.model.recompute_growth();

        rows::commit(&database, state.clone(), &changes).expect("seed 65 intents");
        drop(database);

        let (machine, _shutdown, _drain) = reopen(&dir);
        let loaded = published(&machine).await;
        assert_eq!(
            loaded.model.intents.len() as u64,
            crate::coordination::model::limits::MAX_INTENT_ROWS,
            "sixty-four terminal intents plus the active one"
        );
        assert!(loaded.model.intents.contains_key(&active));
    }

    // ── The batch ───────────────────────────────────────────────────────────

    /// A mixed batch produces one reply per entry, in order, and commits once.
    #[tokio::test]
    async fn a_mixed_batch_answers_every_entry_and_commits_together() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        let replies = machine
            .apply([
                blank(1),
                normal(2, acquire(1)),
                membership_entry(3, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
                normal(4, acquire(2)),
            ])
            .await
            .expect("apply");

        assert_eq!(replies.len(), 4, "one response per applied entry");
        assert!(matches!(replies[0], CommandReplyV1::NonCommand(_)));
        assert!(matches!(replies[1], CommandReplyV1::Client(_)));
        assert!(matches!(replies[2], CommandReplyV1::NonCommand(_)));
        assert!(matches!(replies[3], CommandReplyV1::Client(_)));

        drop(shutdown);
        drop(machine);
        let (mut machine, _shutdown, _drain) = reopen(&dir);
        let (applied, membership) = machine.applied_state().await.expect("applied state");
        assert_eq!(
            applied.map(|id| id.index),
            Some(4),
            "the whole batch landed"
        );
        assert_eq!(membership.log_id().map(|id| id.index), Some(3));
        let state = published(&machine).await;
        assert_eq!(state.model.audit.len(), 2, "one row per *command* entry");
    }

    /// The running store's membership projection is exactly what a restart
    /// derives.
    ///
    /// This is the divergence §7.3 forbids. `members` and `membership_log_id`
    /// are derived from the stored `StoredMembership` at every open, so any
    /// path that writes them from something else -- a controller's progress
    /// report was one -- gives the group two answers to "who are the voters",
    /// and only one of them survives a reboot.
    ///
    /// Asserted over a batch that mixes a membership entry with commands,
    /// rather than about one code path, so a future writer of those fields
    /// fails here whatever route it takes.
    #[tokio::test]
    async fn the_membership_projection_survives_a_restart_unchanged() {
        use crate::coordination::model::membership::{PHASE_ADDING_LEARNERS, PHASE_PENDING};
        use crate::coordination::model::state::{Intent, MembershipPlan};

        let dir = scratch_dir();
        // A real intent, in a phase a progress report can advance. Without one
        // the reports below are no-ops that never reach `write_intent`, and the
        // rule they are meant to guard is never exercised -- which is how a
        // reintroduced non-native write of `membership_log_id` survived
        // mutation with every test green.
        let database = seeded(&dir);
        let mut seed_state = genesis_state();
        seed_state.model.last_applied = Some(LogId::new(1, LEADER, 1));
        let mut changes = ChangeSet::default();
        changes
            .admins
            .extend(seed_state.model.admins.iter().copied());
        changes.grants.extend(seed_state.model.acl.keys().cloned());
        changes
            .resources
            .extend(seed_state.model.resources.keys().cloned());
        let operation = [0x78u8; 16];
        seed_state.model.intents.insert(
            operation,
            Intent {
                actor: ACTOR,
                request_hash: [0x79u8; 32],
                expected_membership: None,
                plan: MembershipPlan {
                    desired_voters: (1u8..=3).map(|b| [b; 32]).collect(),
                    retained_learners: Vec::new(),
                    member_seeds: Vec::new(),
                },
                status_code: PHASE_PENDING,
                result_code: 0,
                accepted_log_id: LogId::new(1, LEADER, 1),
                completed_log_id: None,
                observed_membership_log_id: None,
            },
        );
        changes.intents.insert(operation);
        seed_state.model.growth_bytes = seed_state.model.recompute_growth();
        rows::commit(&database, seed_state.clone(), &changes).expect("seed a pending intent");
        drop(database);

        let (mut machine, shutdown, _drain) = reopen(&dir);

        machine
            .apply([
                normal(2, acquire(1)),
                membership_entry(3, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
                normal(4, acquire(2)),
                // A report that **does** reach `write_intent`, carrying an
                // observation that is not the committed membership. §7.3 lets
                // an intent record what a controller claims to have seen; it
                // must not let that claim become authority.
                normal(
                    5,
                    CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                        operation_id: operation.to_vec(),
                        expected_phase: PHASE_PENDING,
                        new_phase: PHASE_ADDING_LEARNERS,
                        // A plausible id -- an entry that really was applied --
                        // but not the one the committed membership names. A
                        // fabricated future id would be caught by
                        // `stamped-log-ids` instead, which would prove that
                        // rule rather than this one.
                        observed_membership_log_id: Some(crate::coordination::contract::LogIdV1 {
                            term: 1,
                            leader_node_id: LEADER.to_vec(),
                            index: 1,
                        }),
                        result_code: 0,
                    }),
                ),
                // A progress report naming an operation that does not exist:
                // a no-op that must still not touch membership authority.
                normal(
                    6,
                    CommandV1::MembershipProgress(MembershipProgressCommandV1 {
                        operation_id: vec![0x77; 16],
                        expected_phase: 3,
                        new_phase: 4,
                        observed_membership_log_id: Some(crate::coordination::contract::LogIdV1 {
                            term: 9,
                            leader_node_id: vec![0xEE; 32],
                            index: 99,
                        }),
                        result_code: 0,
                    }),
                ),
            ])
            .await
            .expect("apply");
        let running = published(&machine).await;

        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let restarted = published(&machine).await;

        assert_eq!(
            running.model.membership, restarted.model.membership,
            "the membership a restart derives must be the one the store was using"
        );
        assert_eq!(
            running.model.membership_log_id, restarted.model.membership_log_id,
            "and so must the membership log id"
        );
        assert_eq!(running.membership, restarted.membership);
        assert_eq!(
            running.model.membership_log_id.map(|id| id.index),
            Some(3),
            "the native entry is what set it, not either report's claim"
        );
        assert_eq!(
            running.model.intents[&operation].observed_membership_log_id,
            Some(LogId::new(1, LEADER, 1)),
            "the observation is recorded on the intent, which is where §5.2 reads it"
        );
        assert_ne!(
            running.model.intents[&operation].observed_membership_log_id,
            running.model.membership_log_id,
            "and it differs from the committed one, or a write of it back would be \
             indistinguishable from doing nothing"
        );
    }

    /// A batch whose commit fails leaves the disk untouched, reports the
    /// failure, and latches the store fatal.
    ///
    /// The case §4.4 is about. Every entry in the batch has already mutated the
    /// working copy by the time the commit is attempted, so this is the exact
    /// shape in which memory could run ahead of disk.
    ///
    /// **What this does not check**, deliberately: whether the working copy was
    /// published in memory. It cannot. A commit failure is always sticky, the
    /// actor latches fatal *before* it replies, and a fatal store refuses
    /// everything afterwards -- so no reader exists that could tell the two
    /// apart. Mutation confirmed it: moving the publication above the commit
    /// passed all 576 tests.
    ///
    /// Two forms carry it instead of a test. `rows::commit` takes the working
    /// state by value and hands it back only on success, so there is nothing to
    /// publish early; and the actor *moved* its state into the batch, so a
    /// failure leaves it holding none at all. The second is what the assertion
    /// below now rests on -- there is no published state, rather than a
    /// published state nobody may read.
    #[tokio::test]
    async fn a_failed_commit_publishes_neither_memory_nor_disk() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);
        machine.apply([normal(1, acquire(1))]).await.expect("first");
        let before = published(&machine).await;

        machine.handle().fail_next_commit().await.expect("arm");
        let err = machine
            .apply([blank(2), normal(3, acquire(2))])
            .await
            .expect_err("the commit fails");
        assert!(
            format!("{err:?}").contains("injected commit failure"),
            "{err:?}"
        );
        assert!(machine.handle().is_fatal(), "durability is in question");

        // There is no published state to answer with: the batch took it and the
        // commit never gave it back. See the note above for why the in-memory
        // ordering itself is unobservable.
        assert!(machine.handle().published().await.is_err());

        // Disk: unchanged, which is what a reopen shows.
        drop(shutdown);
        drop(machine);
        let (machine, _shutdown, _drain) = reopen(&dir);
        let after = published(&machine).await;
        assert_eq!(
            after.model.last_applied, before.model.last_applied,
            "the failed batch left no trace on disk"
        );
        assert_eq!(after.model.audit.len(), before.model.audit.len());
        assert_eq!(after.model.resources, before.model.resources);
    }

    /// An empty batch is a no-op rather than an empty transaction.
    #[tokio::test]
    async fn an_empty_batch_writes_nothing() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        let before = published(&machine).await;
        let replies = machine.apply(Vec::<Entry>::new()).await.expect("apply");
        assert!(replies.is_empty());
        assert_eq!(published(&machine).await, before);
    }

    /// Once fatal, a *later* call is refused.
    ///
    /// This is the caller-side latch only. Each call below is issued after the
    /// failure has already been reported, so [`StateHandle::call`] rejects it
    /// without sending -- nothing here reaches the actor, and nothing here says
    /// anything about the drain. The test below is the one that does.
    #[tokio::test]
    async fn a_fatal_store_refuses_every_later_command() {
        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        machine
            .handle()
            .inject_fatal()
            .await
            .expect_err("the injected failure");

        assert!(machine.handle().is_fatal());
        assert!(machine.applied_state().await.is_err());
        assert!(machine.apply([blank(1)]).await.is_err());
        assert!(machine.handle().published().await.is_err());
    }

    /// A batch queued *behind* a fatal failure is refused and never applied.
    ///
    /// This is the drain, tested directly. The ordering is forced rather than
    /// raced: the actor is paused, the failure and the batch are enqueued in
    /// that exact order while it cannot service either, and only then is it
    /// released. So the batch is unambiguously in the channel when the failure
    /// is classified -- the case the test above cannot reach, because a caller
    /// checks the latch before sending and therefore never enqueues anything
    /// once the store is fatal.
    ///
    /// Both halves are asserted, and the second is the one with teeth: the
    /// batch is refused, *and* its effect is absent from disk. An actor that
    /// keeps serving after a fatal command fails on the disk check, which no
    /// caller-side latch can prevent.
    #[tokio::test]
    async fn a_batch_queued_behind_a_fatal_failure_is_never_applied() {
        let dir = scratch_dir();
        let before;

        {
            let (machine, shutdown, _drain) = machine(&dir);
            let handle = machine.handle().clone();
            before = published(&machine).await;

            let release = handle.pause().await;

            // Enqueued in this order, into a channel the actor cannot drain.
            let fatal_rx = handle.enqueue(StateCommand::InjectFatal).await;
            let apply_rx = handle
                .enqueue(|reply| StateCommand::Apply {
                    entries: vec![normal(1, acquire(1))],
                    reply,
                })
                .await;

            release.send(()).expect("release the actor");

            assert!(
                fatal_rx.await.expect("reply").is_err(),
                "the injected failure is reported"
            );
            assert!(
                apply_rx.await.expect("reply").is_err(),
                "a batch queued behind a fatal failure must not be acknowledged"
            );
            assert!(handle.is_fatal());

            drop(shutdown);
            drop(machine);
        }

        // And it must not have reached the disk.
        let after = durable(&dir);
        assert_eq!(
            after.model.last_applied, before.model.last_applied,
            "the queued batch was applied despite the store already being fatal"
        );
        assert_eq!(after.model.resources, before.model.resources);
        assert_eq!(after.model.audit.len(), before.model.audit.len());
    }

    /// The store's authority is what the group's rows are bound to, so a state
    /// whose stored key is not a key never reaches an actor.
    #[tokio::test]
    async fn a_state_that_fails_validation_never_publishes_an_actor() {
        let dir = scratch_dir();
        let database = seeded(&dir);
        let mut state = genesis_state();
        // Scanned rather than guessed: roughly half of all 32-byte strings
        // decompress, and `[0x01; 32]` -- the obvious choice -- is one of them.
        let not_a_key = (0u16..=255)
            .map(|n| [n as u8; 32])
            .find(|bytes| ed25519_dalek::VerifyingKey::from_bytes(bytes).is_err())
            .expect("some repeated byte is not a curve point");
        state.model.authority = GroupAuthority {
            public_key: not_a_key,
            ..state.model.authority
        };
        rows::commit(&database, state.clone(), &ChangeSet::default()).expect("write the bad meta");
        drop(database);

        let database = Database::create(dir.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&dir))
            .expect_err("validation refuses it");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    // ── Snapshots (§11.4) ────────────────────────────────────────────────────

    /// The published body's path, from the id it is advertised under.
    /// The bytes a handle names, read the way production reads them.
    ///
    /// Through the handle rather than `std::fs::read` of a path: a path is what
    /// the role-safe reader exists to keep out of callers' hands, and a test
    /// that reached round it would stop noticing if the pin regressed.
    fn body_of(handle: &CoordinationSnapshotFile) -> Vec<u8> {
        use std::io::Read as _;

        let mut bytes = Vec::new();
        handle
            .reader()
            .expect("a readable body")
            .read_to_end(&mut bytes)
            .expect("read the body");
        bytes
    }

    fn body_path(dir: &std::path::Path, snapshot_id: &str) -> PathBuf {
        let stem = super::snapshot::file_stem_for(snapshot_id).expect("a content-derived id");
        dir.join("snapshots")
            .join(format!("{stem}{}", super::snapshot::SNAPSHOT_FILE_SUFFIX))
    }

    /// Build, publish, and still be current after a restart.
    ///
    /// The reopen is the point. The in-memory `current` is rebuilt from
    /// `snapshots/CURRENT` and re-verified against the body, so a marker that
    /// was written but not fsynced, a body under the wrong name, or metadata
    /// that does not match what the body carries all fail here rather than at
    /// the first follower that asks for a snapshot.
    #[tokio::test]
    async fn a_built_snapshot_is_published_and_still_current_after_a_reopen() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        assert!(
            machine
                .get_current_snapshot()
                .await
                .expect("read")
                .is_none(),
            "a store that has never snapshotted has no current snapshot, and \
             that is not an error"
        );

        machine
            .apply([
                blank(1),
                membership_entry(2, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
            ])
            .await
            .expect("apply");

        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            let snapshot = builder.build_snapshot().await.expect("build");
            snapshot.meta.clone()
        };

        assert_eq!(
            built.last_log_id.map(|id| id.index),
            Some(2),
            "the snapshot is taken at the applied index, not at the last command"
        );
        assert!(
            body_path(&dir, &built.snapshot_id).exists(),
            "the body is published under its digest"
        );
        assert!(dir.join("snapshots").join("CURRENT").exists());

        drop(shutdown);
        let (mut machine, shutdown, _drain) = reopen(&dir);
        let after = machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("the marker survives the restart");
        assert_eq!(after.meta, built);
        assert_eq!(
            body_of(&after.snapshot),
            std::fs::read(body_path(&dir, &built.snapshot_id)).expect("the published body"),
            "the handle reads the body the restored marker names"
        );
        assert!(
            !after.snapshot.deletes_on_drop(),
            "what a reader is handed is a Published lease; dropping it must not \
             take this voter's own snapshot with it"
        );
        drop(after);
        drop(shutdown);
    }

    /// The advertised id is BLAKE3 of the published file, computed here rather
    /// than asked of the code that produced it.
    ///
    /// Without an independent oracle the digest only has to be *consistent*: a
    /// writer that hashed the header alone and a reader that did the same would
    /// agree with each other, publish stable ids, survive restarts, and give
    /// two voters at the same state the same id — every other assertion here
    /// would still pass while the id said nothing about the rows.
    #[tokio::test]
    async fn the_snapshot_id_is_blake3_of_the_published_file() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);
        machine
            .apply([
                blank(1),
                membership_entry(2, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
            ])
            .await
            .expect("apply");

        let mut builder = machine.get_snapshot_builder().await;
        let snapshot = builder.build_snapshot().await.expect("build");
        let bytes = body_of(&snapshot.snapshot);

        let expected = format!(
            "{}{}",
            super::snapshot::SNAPSHOT_ID_PREFIX,
            blake3::hash(&bytes).to_hex()
        );
        assert_eq!(snapshot.meta.snapshot_id, expected);
        assert!(
            bytes.len() > 64,
            "a body with rows in it, so hashing only the header would differ"
        );
        drop(snapshot);
        drop(shutdown);
    }

    /// Two stores in different directories with the same replicated state emit
    /// byte-identical bodies and the same content id.
    ///
    /// This is what makes equal-index idempotence sound: if a body could carry
    /// anything node-local, two healthy voters at the same `last_applied` would
    /// disagree on the digest and §11.4's equal-index rule would call it
    /// corruption. `the_local_node_id_is_absent_from_replicated_state` is the
    /// other half — it keeps the one node-local field out of `state.redb`,
    /// which is the only thing this body is a function of.
    #[tokio::test]
    async fn two_stores_at_the_same_applied_state_emit_the_same_bytes() {
        let mut ids = Vec::new();
        let mut bodies = Vec::new();
        let mut dirs = Vec::new();
        for _ in 0..2 {
            let dir = scratch_dir();
            let (mut machine, shutdown, _drain) = machine(&dir);
            machine
                .apply([
                    blank(1),
                    membership_entry(2, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
                ])
                .await
                .expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            let snapshot = builder.build_snapshot().await.expect("build");
            ids.push(snapshot.meta.snapshot_id.clone());
            bodies.push(body_of(&snapshot.snapshot));
            drop(snapshot);
            drop(shutdown);
            dirs.push(dir);
        }

        assert_ne!(dirs[0], dirs[1], "the two stores are genuinely separate");
        assert_eq!(ids[0], ids[1]);
        assert_eq!(bodies[0], bodies[1]);
        assert!(!bodies[0].is_empty());
    }

    /// A builder that finishes behind the current snapshot cannot move the
    /// marker back.
    ///
    /// The stale builder is real rather than injected: the read transaction is
    /// taken before the second entry applies, so streaming it afterwards
    /// produces exactly the body §11.4 describes — one that was correct when it
    /// started and is behind by the time it finishes.
    #[tokio::test]
    async fn an_out_of_order_builder_cannot_regress_the_marker() {
        let dir = scratch_dir();
        let layout = StorageLayout::new(&dir);
        let (mut machine, shutdown, drain) = machine(&dir);

        machine.apply([blank(1)]).await.expect("apply");
        let stale = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction at index 1");
        machine.apply([blank(2)]).await.expect("apply");

        let newer = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        assert_eq!(newer.last_log_id.as_ref().map(|id| id.index), Some(2));

        let older = super::snapshot::build(&layout, &stale).expect("stream the stale transaction");
        // The same shape the builder uses: the version pin ends, the lease that
        // says this store still has work outstanding does not.
        let lease = stale.release_transaction();
        assert_eq!(
            older.meta.last_log_id.as_ref().map(|id| id.index),
            Some(1),
            "the stale transaction still sees the version it pinned"
        );
        let discarded = older.path.clone();

        let serving = machine.publish(&lease, older).expect("publication decides");
        assert_eq!(
            serving.meta.snapshot_id, newer.snapshot_id,
            "the current snapshot is unchanged"
        );
        assert!(
            !discarded.exists(),
            "a discarded body is removed rather than left to accumulate"
        );

        // Closed in the order the store closes in: the builder's read ends,
        // then the drain returns, then the actor releases `state.redb`. Reading
        // the marker back needs the database, so the test cannot get there
        // without performing exactly that sequence.
        drop(lease);
        drop(drain);
        drop(shutdown);
        assert_eq!(
            reload_current(&layout, &dir)
                .expect("load")
                .expect("still published")
                .meta,
            crate::coordination::raft::SnapshotMetaV1::from(&newer),
            "and the durable marker still names the newer snapshot"
        );
    }

    /// A read reports the version *its* transaction pins, not the store's.
    ///
    /// The distinction §11.4's read cut turns on, and it is invisible until the
    /// two disagree: an older transaction is held across a later apply, so the
    /// actor's applied state moves to 2 while the transaction still contains 1.
    /// An implementation that answered from actor memory -- the obvious
    /// shortcut, and one with the same Rust type -- would report 2 for a
    /// transaction that will only ever show 1, and a barrier at 2 would be
    /// satisfied by a read that cannot serve it.
    ///
    /// The second half is the ordinary case: a transaction taken *after* the
    /// barrier is ahead of it, and must be accepted rather than required to
    /// match.
    #[tokio::test]
    async fn a_read_answers_the_barrier_from_its_own_pinned_version() {
        use super::super::reads::BarrierFit;
        use crate::coordination::model::state::LogId as ModelLogId;

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        machine.apply([blank(1)]).await.expect("apply");
        let older = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction at index 1");
        assert_eq!(
            older.pinned().expect("meta").log_id().map(|id| id.index),
            Some(1)
        );

        machine.apply([blank(2)]).await.expect("apply");
        assert_eq!(
            machine
                .applied_state()
                .await
                .expect("applied")
                .0
                .map(|id| id.index),
            Some(2),
            "the store has moved on underneath the held transaction"
        );

        let barrier = ModelLogId::new(1, LEADER, 2);
        assert_eq!(
            older.pinned().expect("meta").log_id().map(|id| id.index),
            Some(1),
            "the held transaction still contains the version it pinned"
        );
        assert_eq!(
            older.pinned().expect("meta").against(Some(&barrier)),
            BarrierFit::Behind,
            "so it cannot serve a read barriered at 2"
        );

        let newer = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction at index 2");
        assert_eq!(
            newer.pinned().expect("meta").against(Some(&barrier)),
            BarrierFit::Satisfied
        );

        // And past the barrier, not merely at it: the transaction taken after a
        // third entry is what a real handler holds, and equality would reject it.
        machine.apply([blank(3)]).await.expect("apply");
        let ahead = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction at index 3");
        assert_eq!(
            ahead.pinned().expect("meta").log_id().map(|id| id.index),
            Some(3)
        );
        assert_eq!(
            ahead.pinned().expect("meta").against(Some(&barrier)),
            BarrierFit::Satisfied
        );
    }

    /// Nothing an outstanding lease holder does needs the actor.
    ///
    /// The premise that makes actor-side quiescence safe. The install has to
    /// wait out readers *while holding the actor still* — the classification
    /// and the rename must see one `last_applied`, and only the actor can
    /// guarantee no apply lands between them. That wait is a deadlock the
    /// moment a holder needs the actor to finish, and the field's own comment
    /// used to claim it did.
    ///
    /// Both holder paths are driven here with the actor paused and unable to
    /// answer anything:
    ///
    /// - a `GuardedRead` streams a complete body and releases its transaction;
    /// - the `ReadLease` that survives it publishes `CURRENT` and is released.
    ///
    /// A regression fails on the bounded wait rather than hanging the suite,
    /// which is why the whole sequence runs on its own thread.
    #[tokio::test]
    async fn a_lease_holder_never_waits_on_the_actor() {
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        // The one actor call a holder makes.
        let read = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction");

        // From here the actor answers nothing at all.
        let release = machine.handle().pause().await;

        let (done, finished) = mpsc::channel();
        let worker = {
            let layout = machine.layout.clone();
            let machine = machine.clone();
            std::thread::spawn(move || {
                let built = snapshot::build(&layout, &read).expect("stream the body");
                let lease = read.release_transaction();
                let published = machine.publish(&lease, built).expect("publish CURRENT");
                drop(lease);
                done.send(published.meta.snapshot_id.clone())
                    .expect("report");
            })
        };

        let id = finished
            .recv_timeout(Duration::from_secs(10))
            .expect("a lease holder completes with the actor unable to answer");
        worker.join().expect("join");
        assert!(id.starts_with("aster-coordination-v1:"));

        // And the gate is free again, which is what a quiescing actor waits for.
        assert_eq!(machine.reads.outstanding(), 0);

        release.send(()).expect("let the actor go");
    }

    // ── Install: receiving (§11.4) ──────────────────────────────────────────

    /// A store with one published snapshot, and a Published handle on it.
    ///
    /// The handle is what a real install is given: OpenRaft's suite moves a
    /// leader's own current snapshot into a follower, so the source is a
    /// foreign store's live file rather than anything this receiver wrote.
    /// The applied command is what fills the last two sections: an `acquire`
    /// writes a client session and an audit row on top of the seeded five.
    async fn a_published_source(
        dir: &std::path::Path,
    ) -> (Store, Snapshot<CoordinationTypeConfig>) {
        let (mut machine, shutdown, drain) = CoordinationStateMachine::new(
            seeded_with(dir, seven_section_state()),
            StorageLayout::new(dir),
        )
        .expect("open a store with every section populated");
        machine
            .apply([normal(2, acquire(1))])
            .await
            .expect("one command");
        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build")
        };
        ((machine, shutdown, drain), built)
    }

    /// A state with a row in **every** §11.4 section.
    ///
    /// Seven sections, because a fixture that leaves one empty cannot tell a
    /// rebuild that skips that table from one that does not: deleting the
    /// `intents` and `routes` writes passed the whole suite while this fixture
    /// had neither.
    ///
    /// Intents and routes are seeded through `rows::commit` rather than driven
    /// by `apply`, and that is a statement about Slice 2 rather than a
    /// shortcut: nothing in this slice writes a route at all. §7.3's controller
    /// is Slice 3's, and it is what will produce them from member seeds.
    fn seven_section_state() -> DurableState {
        use crate::coordination::model::membership::{test_ticket, PHASE_PENDING};
        use crate::coordination::model::state::{Intent, MembershipPlan, Route};

        let voters: Vec<[u8; 32]> = (1u8..=3).map(|b| [b; 32]).collect();
        let mut state = genesis_state();
        state.model.membership =
            crate::coordination::model::state::CommittedMembership::uniform(voters.clone());

        let operation = [0x42u8; 16];
        state.model.intents.insert(
            operation,
            Intent {
                actor: ACTOR,
                request_hash: [0x43u8; 32],
                expected_membership: None,
                plan: MembershipPlan {
                    desired_voters: voters.clone(),
                    retained_learners: Vec::new(),
                    member_seeds: Vec::new(),
                },
                status_code: PHASE_PENDING,
                result_code: 0,
                accepted_log_id: LogId::new(1, LEADER, 1),
                completed_log_id: None,
                observed_membership_log_id: None,
            },
        );
        // A route's node must be a member: §9 protects one by asking exactly
        // that, so a route for a stranger is a state the validator refuses.
        for node in &voters {
            state.model.routes.insert(
                *node,
                Route {
                    ticket: test_ticket(node),
                    source_operation_id: operation,
                    introduced_log_id: LogId::new(1, LEADER, 1),
                },
            );
        }
        // The intent and the routes are stamped at index 1, so the state has
        // to have applied it: a log id stamped on a row of a state that has
        // applied nothing is a state `validate` refuses, and rightly.
        state.model.last_applied = Some(LogId::new(1, LEADER, 1));
        state.model.growth_bytes = state.model.recompute_growth();
        state.model.reserve_bytes = state.model.recompute_reserve();
        state
    }

    /// Assert every §11.4 section carries at least one row.
    fn assert_seven_sections(state: &DurableState, when: &str) {
        for (what, empty) in [
            ("resources", state.model.resources.is_empty()),
            ("grants", state.model.acl.is_empty()),
            ("admins", state.model.admins.is_empty()),
            ("sessions", state.model.sessions.is_empty()),
            ("intents", state.model.intents.is_empty()),
            ("routes", state.model.routes.is_empty()),
            ("audit", state.model.audit.is_empty()),
        ] {
            assert!(!empty, "{what} is empty {when}");
        }
    }

    /// Install reads its source and leaves it exactly as it found it.
    ///
    /// The property §11.4 states and the one an implementation is most likely
    /// to break for convenience: the obvious way to install a body is to rename
    /// it into place, and the source here belongs to a *different store* that is
    /// still serving it. So the receiver takes a copy, into its own `incoming/`,
    /// and the source's bytes and directory entry are asserted untouched
    /// afterwards.
    #[tokio::test]
    async fn an_install_source_is_copied_and_never_touched() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let source_path = body_path(&source_dir, &published.meta.snapshot_id);
        let source_bytes = std::fs::read(&source_path).expect("the source body");

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("a foreign store's published snapshot is an install source");

        assert!(
            staged.path().starts_with(dir.join("incoming")),
            "the receiver stages under its own incoming/, not beside the source: {}",
            staged.path().display()
        );
        assert_eq!(
            std::fs::read(staged.path()).expect("the staged body"),
            source_bytes,
            "and stages the source byte for byte"
        );
        assert_eq!(staged.len(), source_bytes.len() as u64);
        assert_eq!(staged.meta().snapshot_id, published.meta.snapshot_id);

        assert!(source_path.exists(), "the source is not consumed");
        assert_eq!(
            std::fs::read(&source_path).expect("the source body"),
            source_bytes,
            "nor mutated"
        );
        assert!(
            source_dir.join("snapshots").join("CURRENT").exists(),
            "nor is the other store's marker disturbed"
        );
    }

    /// A body from another group is refused, and refused on identity rather
    /// than on being ahead.
    ///
    /// Install's direction rule is the opposite of a load's -- an installable
    /// snapshot is normally ahead of `state.redb` -- so the identity half had to
    /// be separable. This is what proves the separation kept its teeth: the
    /// source is both ahead *and* foreign, and it must be refused for being
    /// foreign.
    #[tokio::test]
    async fn a_source_bound_to_another_group_is_refused() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let mut elsewhere = genesis_state();
        elsewhere.model.authority.group_id = [0x5a; 32];
        let (machine, _shutdown, _drain) =
            CoordinationStateMachine::new(seeded_with(&dir, elsewhere), StorageLayout::new(&dir))
                .expect("a store for a different group");

        let err = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect_err("another group's state is not installable here");
        assert!(format!("{err:?}").contains("group id"), "{err:?}");
        assert!(
            leftovers(&dir).is_empty(),
            "and a refused install stages nothing that survives it"
        );
    }

    /// The supplied metadata is checked against the body, not believed.
    ///
    /// All three fields, because a check that covered two of them would pass
    /// every test that used a consistent fixture. Each case starts from the
    /// metadata that *does* match and changes exactly one field.
    #[tokio::test]
    async fn supplied_metadata_that_disagrees_with_the_body_is_refused() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let truthful = crate::coordination::raft::SnapshotMetaV1::from(&published.meta);

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let state = durable_of(&machine).await;
        let layout = StorageLayout::new(&dir);

        let refused = |supplied: crate::coordination::raft::SnapshotMetaV1, what: &str| {
            let err =
                super::super::snapshot::receive(&layout, &published.snapshot, &state, &supplied)
                    .expect_err("supplied metadata is not evidence about the body");
            assert!(format!("{err:?}").contains(what), "{err:?}");
        };

        let mut wrong_id = truthful.clone();
        wrong_id.snapshot_id = super::super::snapshot::snapshot_id_for(&[0x99; 32]);
        refused(wrong_id, "hashes to");

        let mut wrong_log_id = truthful.clone();
        wrong_log_id.last_log_id = Some(crate::coordination::contract::LogIdV1 {
            term: 9,
            leader_node_id: LEADER.to_vec(),
            index: 9,
        });
        refused(wrong_log_id, "last log id");

        let mut wrong_membership = truthful.clone();
        wrong_membership.last_membership.membership.configs =
            vec![crate::coordination::raft::NodeIdSetV1 {
                ids: vec![crate::coordination::raft::NodeIdV1 {
                    node_id: LEADER.to_vec(),
                }],
            }];
        refused(wrong_membership, "membership");

        // The truthful copy still works, so the three refusals above are about
        // the fields changed and not about the fixture.
        super::super::snapshot::receive(&layout, &published.snapshot, &state, &truthful)
            .expect("the metadata the body actually implies");
    }

    /// A body rebuilds the state it was streamed from, row for row.
    ///
    /// The independent oracle is the *source store's* durable state: the
    /// rebuild is compared against what the sender's database actually held,
    /// not against anything the rebuild computed. A conversion that dropped a
    /// field, or reconstructed one from another, disagrees here even though
    /// the body it produced would still round-trip through itself.
    ///
    /// Deliberately a state with rows in all seven sections. Comparing two
    /// empty databases would pass under a rebuild that inserted nothing, and
    /// leaving one section empty would not witness that section's mapping.
    #[tokio::test]
    async fn a_body_rebuilds_the_state_it_was_streamed_from() {
        let source_dir = scratch_dir();
        let ((source, _source_shutdown, _source_drain), published) =
            a_published_source(&source_dir).await;
        let before = durable_of(&source).await;
        assert_seven_sections(&before, "before the rebuild");

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage the source");

        // The candidate builds and verifies itself: `rebuild_candidate` ends by
        // holding the file to production's own open, so this call succeeding is
        // already the reopen assertion. A candidate missing `codec_version`
        // fails here rather than after it has replaced `state.redb`.
        let verified = super::super::snapshot::rebuild_candidate(&StorageLayout::new(&dir), staged)
            .expect("the candidate rebuilds and opens as production would");
        let rebuilt = candidate_in(&dir).expect("the candidate is under incoming/");
        assert_seven_sections(verified.rebuilt(), "after the rebuild");
        assert_eq!(
            verified.rebuilt(),
            &before,
            "the rebuilt state is the state the body was streamed from"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&rebuilt)
                    .expect("candidate metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "a candidate gets the same owner-only mode as state.redb"
            );
        }
    }

    /// Row-valid input is not necessarily a state production can open.
    ///
    /// The body below is canonical and every individual row decodes. Its only
    /// defect is a persisted aggregate counter that disagrees with those rows.
    /// Receiving therefore succeeds, and the rebuild itself must run the full
    /// `rows::load` validation before the candidate can replace anything.
    #[tokio::test]
    async fn a_rebuild_refuses_row_valid_input_with_invalid_whole_state() {
        let valid_dir = scratch_dir();
        let ((valid, _valid_shutdown, _valid_drain), _published) =
            a_published_source(&valid_dir).await;
        let mut invalid = durable_of(&valid).await;
        invalid.model.growth_bytes = invalid
            .model
            .growth_bytes
            .checked_add(1)
            .expect("the test counter has headroom");

        // `rows::commit` deliberately writes the caller's committed state; it
        // is not the loader's independent validator. That lets this fixture
        // produce the exact on-disk corruption the production load must catch.
        let source_dir = scratch_dir();
        let database = seeded_with(&source_dir, invalid);
        let read = database.begin_read().expect("read the invalid source");
        let built = super::super::snapshot::build(&StorageLayout::new(&source_dir), &read)
            .expect("row-valid state still has a canonical body");
        drop(read);
        drop(database);

        let supplied = built.meta.clone();
        let source = CoordinationSnapshotFile::incoming(built.path);
        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &source,
            &durable_of(&machine).await,
            &supplied,
        )
        .expect("row decoding and body binding both succeed");

        let err = super::super::snapshot::rebuild_candidate(&StorageLayout::new(&dir), staged)
            .expect_err("whole-state validation is part of rebuilding");
        let detail = format!("{err:?}");
        assert!(detail.contains("aggregate-counters"), "{detail}");
        assert!(detail.contains("growth persisted"), "{detail}");
    }

    /// A candidate is never the state database it will replace.
    ///
    /// The regression for a hole that was structural rather than logical:
    /// `replace` used to take the target as an argument, and the caller of
    /// `rebuild_candidate` already knew the candidate's path. Passing it back
    /// renames a file over itself -- which succeeds, as a no-op -- so cleanup
    /// was disarmed, success was reported, and `state.redb` was untouched. An
    /// install that silently did nothing.
    ///
    /// Both ends now come from one `StorageLayout`, so there is no argument to
    /// pass. What remains testable is that the two derived names really are
    /// different, and that a replacement really does move the bytes.
    #[tokio::test]
    async fn a_replacement_moves_the_candidate_onto_the_state_database() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, shutdown, drain) = machine(&dir);
        let layout = StorageLayout::new(&dir);
        let staged = super::super::snapshot::receive(
            &layout,
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage the source");
        let verified = granted(
            super::super::snapshot::rebuild_candidate(&layout, staged).expect("a valid candidate"),
            &durable_of(&machine).await,
        );

        // The store's own handles go first: that is what the install's quiesce
        // is for, and a rename over an open database is not what is being
        // tested here.
        drop(machine);
        drop(drain);
        drop(shutdown);

        let target = dir.join("state.redb");
        let old = std::fs::read(&target).expect("the state this store was serving");
        let candidate = candidate_in(&dir).expect("one candidate, under incoming/");
        let candidate_bytes = std::fs::read(&candidate).expect("the candidate's bytes");
        assert_ne!(
            candidate, target,
            "a candidate that named its own target would replace nothing"
        );
        assert_ne!(candidate_bytes, old, "and the two differ, so a no-op shows");

        verified.replace().expect("the replacement succeeds");
        assert_eq!(
            std::fs::read(&target).expect("the state database"),
            candidate_bytes,
            "the state database is now the candidate's bytes, not the old ones"
        );
        assert!(
            candidate_in(&dir).is_none(),
            "and no candidate is left under incoming/"
        );
    }

    /// The rows rebuilt are the rows that were verified.
    ///
    /// Install verifies a body and then reads it again to rebuild, so the two
    /// reads have to be provably of the same bytes. The earlier shape took a
    /// header and a path as separate arguments and discarded the header the
    /// body carried, which let a header from one body be paired with the rows
    /// of another: identity checked against the first, counts matched the
    /// second, and the installed state was neither.
    ///
    /// Driven by replacing the staged file with a *different valid body*
    /// between the two steps -- a real time-of-check/time-of-use, not a
    /// corruption -- so nothing about the framing is wrong and only the
    /// identity of the bytes has changed.
    #[tokio::test]
    async fn a_body_swapped_after_verification_is_refused() {
        let first_dir = scratch_dir();
        let (_first, first) = a_published_source(&first_dir).await;

        // A second, different body: another store, one more command applied.
        let second_dir = scratch_dir();
        let ((mut second, _s2, _d2), _) = a_published_source(&second_dir).await;
        second
            .apply([normal(3, acquire(2))])
            .await
            .expect("diverge");
        let other = {
            let mut builder = second.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build")
        };
        assert_ne!(first.meta.snapshot_id, other.meta.snapshot_id);

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &first.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&first.meta),
        )
        .expect("stage the first body");

        std::fs::copy(
            body_path(&second_dir, &other.meta.snapshot_id),
            staged.path(),
        )
        .expect("swap the staged bytes under the verified handle");

        let err = super::super::snapshot::rebuild_candidate(&StorageLayout::new(&dir), staged)
            .expect_err("the rebuilt rows must be the verified rows");
        assert!(format!("{err:?}").contains("verified"), "{err:?}");
    }

    /// A row edited without disturbing the header is refused too.
    ///
    /// The companion to the swap above, and the reason both checks exist.
    /// Mutation testing showed the swap is caught by the header comparison
    /// alone -- the digest check never runs -- so on its own it would leave the
    /// digest check unevidenced. This is the case the header *cannot* see: one
    /// byte inside a route's ticket, so every count, every section boundary and
    /// every ordering key is exactly as verified, and only the bytes differ.
    #[tokio::test]
    async fn a_row_edited_after_verification_is_refused() {
        use crate::coordination::model::membership::test_ticket;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage the source");

        let mut bytes = std::fs::read(staged.path()).expect("the staged body");
        let ticket = test_ticket(&[1u8; 32]);
        let at = bytes
            .windows(ticket.len())
            .position(|w| w == ticket.as_slice())
            .expect("the route's ticket is in the body");
        // Base58 to base58: still a legal string of the same length, so nothing
        // structural changes.
        bytes[at] = if bytes[at] == b'A' { b'B' } else { b'A' };
        std::fs::write(staged.path(), &bytes).expect("edit one row in place");

        let err = super::super::snapshot::rebuild_candidate(&StorageLayout::new(&dir), staged)
            .expect_err("the rebuilt bytes must hash to what was verified");
        assert!(format!("{err:?}").contains("hashed to"), "{err:?}");
    }

    /// A failed rebuild leaves the path free for the retry.
    ///
    /// Two rules that trap each other if cleanup is left to the error paths:
    /// creation refuses an existing path, so a rebuild that failed *after*
    /// creating the file makes every later attempt at that path fail too — with
    /// `AlreadyExists`, naming the leftovers of the first attempt instead of
    /// the reason it failed. The candidate therefore removes its file on drop,
    /// and the same path is usable immediately afterwards.
    ///
    /// The first attempt fails inside verification, which is the interesting
    /// side: the file exists, is a complete redb database, and has been written
    /// to — the state is what is wrong with it.
    #[tokio::test]
    async fn a_failed_rebuild_leaves_its_path_free_for_a_retry() {
        use redb::ReadableDatabase as _;

        // A body whose rows are all valid and whose counters are not.
        let valid_dir = scratch_dir();
        let ((valid, _vs, _vd), _) = a_published_source(&valid_dir).await;
        let mut broken = durable_of(&valid).await;
        broken.model.growth_bytes = broken.model.growth_bytes.saturating_add(1);
        let broken_dir = scratch_dir();
        let database = seeded_with(&broken_dir, broken);
        let read = database.begin_read().expect("read");
        let bad = super::super::snapshot::build(&StorageLayout::new(&broken_dir), &read)
            .expect("a canonical body");
        drop(read);
        drop(database);

        let dir = scratch_dir();
        let (machine, shutdown, drain) = machine(&dir);
        let state = durable_of(&machine).await;
        let layout = StorageLayout::new(&dir);

        let staged_bad = super::super::snapshot::receive(
            &layout,
            &CoordinationSnapshotFile::incoming(bad.path),
            &state,
            &bad.meta,
        )
        .expect("the bad body stages");
        let bad_body = staged_bad.path().to_path_buf();
        super::super::snapshot::rebuild_candidate(&layout, staged_bad)
            .expect_err("its counters do not describe its rows");
        assert!(
            candidate_in(&dir).is_none(),
            "a candidate that failed verification must not survive it"
        );
        assert!(
            !bad_body.exists(),
            "and the staged body goes with it: the prepared install owned both"
        );

        // Immediately afterwards, with a body that is sound.
        let good_dir = scratch_dir();
        let (_good, good) = a_published_source(&good_dir).await;
        let staged_good = super::super::snapshot::receive(
            &layout,
            &good.snapshot,
            &state,
            &crate::coordination::raft::SnapshotMetaV1::from(&good.meta),
        )
        .expect("the good body stages");
        let verified = granted(
            super::super::snapshot::rebuild_candidate(&layout, staged_good)
                .expect("the retry is not blocked by the first attempt's leftovers"),
            &state,
        );
        assert_seven_sections(verified.rebuilt(), "after the retry");

        // And nothing removes it once a replacement has taken it.
        drop(machine);
        drop(drain);
        drop(shutdown);
        verified.replace().expect("the rename succeeds");
        assert!(
            layout.database(Db::State).exists(),
            "the candidate is now the state database"
        );
    }

    /// Every outcome of the install decision, driven from real stores.
    ///
    /// The rule is monotonic by applied index, with full identity **and**
    /// content at an equal one. Each case is reached by moving the *receiver*
    /// rather than by constructing a candidate: a fixture that hand-built the
    /// metadata would be asserting against itself, and the equal case in
    /// particular only means anything when both sides really did apply the same
    /// entries.
    ///
    /// Each case prepares a real candidate, because that is the order the
    /// install runs in and because the decision is a method on the prepared
    /// value -- there is no staged body to classify on its own.
    #[tokio::test]
    async fn the_install_decision_covers_stale_equal_and_higher() {
        use super::super::snapshot::ResolvedInstall;

        // A source at applied index 2.
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        // A receiver at applied index 1: the seeded seven-section state, with
        // nothing applied on top.
        let receiver = |dir: std::path::PathBuf| {
            let (machine, shutdown, drain) = CoordinationStateMachine::new(
                seeded_with(&dir, seven_section_state()),
                StorageLayout::new(&dir),
            )
            .expect("open");
            (machine, shutdown, drain, dir)
        };

        // ── Higher: the receiver is behind the candidate ────────────────────
        {
            let dir = scratch_dir();
            let (machine, _s, _d, dir) = receiver(dir);
            let durable = durable_of(&machine).await;
            let prepared = prepare(
                &dir,
                &published.snapshot,
                &durable,
                &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
            );
            let resolved = prepared
                .resolve(&durable, || panic!("no digest at a higher index"))
                .expect("resolve");
            assert!(
                matches!(resolved, ResolvedInstall::Replace(_)),
                "index 2 over index 1 replaces: {resolved:?}"
            );
            // And only this arm can. The others do not carry a prepared
            // install, so `replace` is not reachable from them at all.
        }

        // ── Stale: the receiver has applied past the candidate ──────────────
        {
            let dir = scratch_dir();
            let (mut machine, _s, _d, dir) = receiver(dir);
            machine
                .apply([blank(2), blank(3)])
                .await
                .expect("move past the candidate");
            let durable = durable_of(&machine).await;
            let prepared = prepare(
                &dir,
                &published.snapshot,
                &durable,
                &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
            );
            let candidate = candidate_in(&dir).expect("a candidate was built");
            let resolved = prepared
                .resolve(&durable, || panic!("no digest when stale"))
                .expect("resolve");
            assert!(
                matches!(resolved, ResolvedInstall::Stale(_)),
                "index 2 under index 3 is a rollback: {resolved:?}"
            );
            assert!(
                !candidate.exists(),
                "a refused install keeps neither artifact"
            );
        }

        // ── Equal, same content: the lost-acknowledgement repair ────────────
        {
            let dir = scratch_dir();
            let (mut machine, _s, _d, dir) = receiver(dir);
            let durable = durable_of(&machine).await;
            // Its own body, at its own applied index: the state it would be
            // asked to install after a lost acknowledgement.
            let mine = {
                let mut builder = machine.get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            let prepared = prepare(
                &dir,
                &mine.snapshot,
                &durable,
                &crate::coordination::raft::SnapshotMetaV1::from(&mine.meta),
            );
            let digest = *prepared.staged().digest();
            let candidate = candidate_in(&dir).expect("a candidate was built");
            let resolved = prepared.resolve(&durable, || Ok(digest)).expect("resolve");
            let ResolvedInstall::Repair(body) = resolved else {
                panic!("the same state arriving again repairs rather than replaces: {resolved:?}");
            };
            assert!(
                !candidate.exists(),
                "the rebuilt database goes: repairing never rewrites the state"
            );
            assert!(
                body.publishable().body().exists(),
                "and the body stays, because CURRENT may still owe a publication"
            );
        }

        // ── Equal index, different log id: divergent history ────────────────
        //
        // Two leaders can write different entries at the same index, so the
        // index matching is not the two sides agreeing about anything. Reached
        // by giving the source's state the same index under a later term:
        // everything else -- group, genesis, authority, profile, index --
        // matches, which is exactly the case the identity checks cannot see.
        {
            let forked_dir = scratch_dir();
            let mut forked = seven_section_state();
            forked.model.last_applied = Some(LogId::new(2, LEADER, 1));
            let database = seeded_with(&forked_dir, forked);
            let read = database.begin_read().expect("read");
            let body = super::super::snapshot::build(&StorageLayout::new(&forked_dir), &read)
                .expect("a canonical body at (term 2, index 1)");
            drop(read);
            drop(database);

            let dir = scratch_dir();
            let (machine, _s, _d, dir) = receiver(dir);
            let durable = durable_of(&machine).await;
            assert_eq!(
                durable.model.last_applied.map(|id| id.index),
                Some(1),
                "the receiver is at the same index"
            );
            let source = CoordinationSnapshotFile::incoming(body.path);
            let prepared = prepare(&dir, &source, &durable, &body.meta);
            let candidate = candidate_in(&dir).expect("a candidate was built");
            let resolved = prepared
                .resolve(&durable, || {
                    panic!("the log ids disagree; there is nothing to hash")
                })
                .expect("resolve");
            assert!(
                matches!(resolved, ResolvedInstall::Fatal(_)),
                "same index under a different term is not the same entry: {resolved:?}"
            );
            assert!(
                !candidate.exists(),
                "divergent history keeps neither artifact"
            );
        }

        // ── Equal index, same log id, different content ─────────────────────
        {
            let dir = scratch_dir();
            let (mut machine, _s, _d, dir) = receiver(dir);
            let durable = durable_of(&machine).await;
            let mine = {
                let mut builder = machine.get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            let prepared = prepare(
                &dir,
                &mine.snapshot,
                &durable,
                &crate::coordination::raft::SnapshotMetaV1::from(&mine.meta),
            );
            let resolved = prepared
                .resolve(&durable, || Ok([0x5a; 32]))
                .expect("resolve");
            assert!(
                matches!(resolved, ResolvedInstall::Fatal(_)),
                "same log id, different bodies: {resolved:?}"
            );
        }
    }

    /// Resolve a prepared install and take the replacement it grants.
    ///
    /// Every test that replaces goes through the verdict, because that is the
    /// only route: `replace` hangs off the capability, not off the prepared
    /// value.
    fn granted(
        prepared: super::super::snapshot::PreparedInstall,
        durable: &DurableState,
    ) -> super::super::snapshot::ReplaceInstall {
        match prepared
            .resolve(durable, || panic!("a higher candidate does not hash"))
            .expect("resolve")
        {
            super::super::snapshot::ResolvedInstall::Replace(granted) => *granted,
            other => panic!("expected a replacement: {other:?}"),
        }
    }

    /// Receive and rebuild, which is what every install does before deciding.
    fn prepare(
        dir: &std::path::Path,
        source: &CoordinationSnapshotFile,
        durable: &DurableState,
        meta: &crate::coordination::raft::SnapshotMetaV1,
    ) -> super::super::snapshot::PreparedInstall {
        let layout = StorageLayout::new(dir);
        let staged = super::super::snapshot::receive(&layout, source, durable, meta)
            .expect("stage the source");
        super::super::snapshot::rebuild_candidate(&layout, staged).expect("rebuild the candidate")
    }

    /// A candidate replaces the store it was built for, and only that one.
    ///
    /// The hole this closes was not that two layouts could be *confused* — it
    /// was that nothing said they could not be. `replace` took a layout, so a
    /// candidate rebuilt and validated against store A's identity and applied
    /// index could be handed store B's layout and overwrite B's `state.redb`
    /// with A's state. Both stores are in the same group here, which is the
    /// hard case: every identity check passes, and only the *destination* is
    /// wrong.
    ///
    /// The cross-store call is now a compile error — `replace` takes no
    /// arguments — so what remains testable is that the bound target is A's and
    /// that B is untouched by A's install.
    #[tokio::test]
    async fn a_candidate_replaces_only_the_store_it_was_built_for() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        // Two stores of the same group, so nothing but the destination differs.
        let a = scratch_dir();
        let b = scratch_dir();
        let (machine_a, shutdown_a, drain_a) = machine(&a);
        let (machine_b, shutdown_b, drain_b) = machine(&b);
        let layout_a = StorageLayout::new(&a);

        let staged = super::super::snapshot::receive(
            &layout_a,
            &published.snapshot,
            &durable_of(&machine_a).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage for A");
        let verified = granted(
            super::super::snapshot::rebuild_candidate(&layout_a, staged)
                .expect("a candidate for A"),
            &durable_of(&machine_a).await,
        );

        drop(machine_a);
        drop(drain_a);
        drop(shutdown_a);
        drop(machine_b);
        drop(drain_b);
        drop(shutdown_b);

        let a_state = a.join("state.redb");
        let b_state = b.join("state.redb");
        let a_before = std::fs::read(&a_state).expect("A's state");
        let b_before = std::fs::read(&b_state).expect("B's state");
        let candidate_bytes =
            std::fs::read(candidate_in(&a).expect("A's candidate")).expect("candidate bytes");

        verified.replace().expect("the replacement succeeds");

        assert_eq!(
            std::fs::read(&a_state).expect("A's state"),
            candidate_bytes,
            "A took the candidate"
        );
        assert_ne!(
            std::fs::read(&a_state).expect("A's state"),
            a_before,
            "and A really changed"
        );
        assert_eq!(
            std::fs::read(&b_state).expect("B's state"),
            b_before,
            "B is byte-identical: a candidate built for A cannot reach it"
        );
    }

    /// A rename that fails removes the candidate and leaves the old state.
    ///
    /// The ordering this exists to pin: cleanup is disarmed *after* the rename,
    /// never before. Disarming first — which is what handing the caller a path
    /// did — leaves a candidate on disk that nothing owns.
    ///
    /// The rename attempted is the real one, against this store's own
    /// `state.redb`. An earlier version aimed at a sealed directory elsewhere,
    /// which never touched the state it claimed to preserve — so it would have
    /// passed against a `replace` that renamed nothing at all. Sealing the
    /// storage root instead makes the destination refuse the rename while the
    /// candidate, one directory down under `incoming/`, is still removable.
    ///
    /// Unix-only because it needs a rename that fails, and a read-only
    /// directory is how one is arranged there.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_failed_replacement_removes_the_candidate_and_keeps_the_old_state() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, shutdown, drain) = machine(&dir);
        let layout = StorageLayout::new(&dir);
        let staged = super::super::snapshot::receive(
            &layout,
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage");
        let verified = granted(
            super::super::snapshot::rebuild_candidate(&layout, staged).expect("a valid candidate"),
            &durable_of(&machine).await,
        );
        let candidate = candidate_in(&dir).expect("the candidate is under incoming/");

        // The store's own handles close first: that is what the install's
        // quiesce is for, and a rename over an open database is a different
        // question.
        drop(machine);
        drop(drain);
        drop(shutdown);
        let target = layout.database(Db::State);
        let before = std::fs::read(&target).expect("the old state database");

        // The root refuses new names, so the rename onto `state.redb` fails.
        // `incoming/` stays writable, so removing the candidate does not.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let err = verified.replace().expect_err("the rename cannot succeed");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(format!("{err:?}").contains("state.redb"), "{err:?}");

        assert!(
            !candidate.exists(),
            "a candidate whose replacement failed is removed, not left behind"
        );
        assert_eq!(
            std::fs::read(&target).expect("the old state database"),
            before,
            "and the state this store was serving is untouched"
        );
    }

    /// A durability failure *after* the rename keeps the new database.
    ///
    /// The other side of the same ordering, and the one where deleting would be
    /// the disaster: the rename has taken effect, so the file is the store's
    /// state now. Reaching the fsync with an error means "installed but not
    /// proven durable", which is a reason to stop the store, not to undo it.
    #[tokio::test]
    async fn a_durability_failure_after_the_rename_keeps_the_new_database() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, shutdown, drain) = machine(&dir);
        let layout = StorageLayout::new(&dir);
        let staged = super::super::snapshot::receive(
            &layout,
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage");
        let verified = granted(
            super::super::snapshot::rebuild_candidate(&layout, staged).expect("a valid candidate"),
            &durable_of(&machine).await,
        );
        let candidate = candidate_in(&dir).expect("the candidate is under incoming/");
        let installed = std::fs::read(&candidate).expect("the candidate's bytes");

        drop(machine);
        drop(drain);
        drop(shutdown);

        super::super::snapshot::fault::fail_next_directory_fsync();
        let err = verified
            .replace()
            .expect_err("the injected post-rename durability failure is reported");
        assert!(format!("{err:?}").contains("injected"), "{err:?}");

        assert_eq!(
            std::fs::read(layout.database(Db::State)).expect("the state database"),
            installed,
            "the replacement stands: undoing it would discard the installed state"
        );
        assert!(
            !candidate.exists(),
            "and the candidate name is free, because the rename did happen"
        );
    }

    /// An abandoned candidate does not outlive the value that owns it.
    #[tokio::test]
    async fn a_candidate_dropped_without_being_kept_is_removed() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage");

        {
            let verified =
                super::super::snapshot::rebuild_candidate(&StorageLayout::new(&dir), staged)
                    .expect("a valid candidate");
            assert!(
                candidate_in(&dir).is_some(),
                "it is on disk while it is held"
            );
            drop(verified);
        }
        assert!(
            candidate_in(&dir).is_none(),
            "an install that gives up after validating still leaves nothing behind"
        );
    }

    /// A candidate that cannot be opened the way production opens a state
    /// database is not a candidate.
    ///
    /// The specific hole this closes: `rows::load` is happy without
    /// `codec_version`, so a rebuild that wrote tables and a meta row looked
    /// complete under every assertion the round-trip test makes — and would
    /// have failed at the reopen *after* replacing `state.redb`, which is the
    /// one moment there is nothing to fall back to.
    #[tokio::test]
    async fn a_candidate_without_the_full_binding_is_refused() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &published.snapshot,
            &durable_of(&machine).await,
            &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
        )
        .expect("stage the source");

        let layout = StorageLayout::new(&dir);
        let meta_row = staged.header().meta.clone();
        super::super::snapshot::rebuild_candidate(&layout, staged)
            .expect("a complete binding opens as production");

        // The same rows and meta row, without the codec version: what the
        // rebuild wrote before `write_state_binding` owned the whole binding.
        let candidate = super::super::schema::create_state_candidate(&layout).expect("create");
        {
            let write = super::super::schema::durable_write(candidate.database(), Db::State)
                .expect("write");
            super::super::schema::create_state_tables(&write).expect("tables");
            {
                let mut table = write
                    .open_table(super::super::schema::STATE_META)
                    .expect("meta");
                let bytes = super::super::codec::encode_committed(
                    Db::State,
                    "CoordinationMetaV1",
                    &meta_row,
                    super::super::schema::MAX_COORDINATION_META_BYTES,
                )
                .expect("encode");
                table
                    .insert(super::super::schema::KEY_META, bytes.as_slice())
                    .expect("insert");
            }
            write.commit().expect("commit");
        }
        let err = super::super::schema::verify_state_database(candidate)
            .expect_err("a state database without its codec version is not openable");
        assert!(format!("{err:?}").contains("codec"), "{err:?}");
    }

    /// A key that is not this schema's width never reaches a table.
    ///
    /// Written after the rebuild's own width check turned out to be
    /// unreachable: `read_body` runs `validate_snapshot_row` before a visitor
    /// sees a row, so a 31-byte node id is refused one layer earlier than the
    /// rebuild. This asserts the layer that *does* catch it rather than the one
    /// that would if it were reached — a body carrying such a row cannot even
    /// be written, which is the strongest form the property takes.
    ///
    /// The rebuild keeps its own conversion regardless; it is what produces the
    /// `[u8; 32]` at all, and leaving its failure to an `unwrap` would put the
    /// width rule in one module with nothing in this one depending on it.
    #[test]
    fn a_row_key_of_the_wrong_width_never_reaches_a_table() {
        use crate::coordination::rowstream::{
            validate_snapshot_row, BodyWriter, GrantRowV1, RowCountsV1, SnapshotBodyRowV1,
            SnapshotHeaderV1,
        };

        let short = SnapshotBodyRowV1::Grant(GrantRowV1 {
            key: crate::coordination::contract::ResourceKeyV1 {
                namespace: "ns".into(),
                key: b"lock".to_vec(),
            },
            // Thirty-one bytes: decodes, orders, and is not a NodeId.
            subject_node_id: vec![0x07; 31],
            rights: 1,
        });
        let err = validate_snapshot_row(&short).expect_err("31 bytes is not a subject");
        assert!(format!("{err:?}").contains("32 bytes"), "{err:?}");

        // And the writer refuses it too, so no such body exists to install.
        let dir = scratch_dir();
        let header = SnapshotHeaderV1 {
            row_counts: RowCountsV1 {
                grants: 1,
                ..Default::default()
            },
            ..header_for(&genesis_state())
        };
        let handle = std::fs::File::create(dir.join("short-key.body")).expect("create");
        let mut writer = BodyWriter::<_, SnapshotBodyRowV1>::new(handle, &header).expect("header");
        writer
            .begin_section(crate::coordination::rowstream::SECTION_GRANTS)
            .expect("section");
        writer
            .row(&short)
            .expect_err("a body cannot be written with a key of the wrong width");
    }

    /// The candidate a rebuild derived, found by the tag its name carries.
    ///
    /// Tests cannot name it: the whole point of deriving both ends from the
    /// layout is that no caller supplies the path. `incoming/` also holds the
    /// staged body, so the tag is what tells them apart.
    fn candidate_in(dir: &std::path::Path) -> Option<PathBuf> {
        leftovers(dir)
            .into_iter()
            .find(|p| p.to_string_lossy().contains("candidate"))
    }

    /// What a store's `incoming/` holds after a failed install.
    fn leftovers(dir: &std::path::Path) -> Vec<PathBuf> {
        match std::fs::read_dir(dir.join("incoming")) {
            Ok(entries) => entries.map(|e| e.expect("entry").path()).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// The receiver's own durable state, as install compares against it.
    async fn durable_of(machine: &CoordinationStateMachine) -> DurableState {
        *machine.handle().published().await.expect("published")
    }

    /// A snapshot header that belongs to this store.
    fn header_for(state: &DurableState) -> crate::coordination::rowstream::SnapshotHeaderV1 {
        crate::coordination::rowstream::SnapshotHeaderV1 {
            format_version: crate::coordination::rowstream::SNAPSHOT_FORMAT_VERSION,
            schema_version: super::super::identity::LOGICAL_SCHEMA_VERSION,
            meta: rows::meta_to_wire(state),
            row_counts: Default::default(),
        }
    }

    /// A body is not accepted merely because it is internally consistent.
    ///
    /// Every check before this one asks whether the marker and the body agree
    /// with each other. They always do in a *complete foreign snapshot*: same
    /// framing, same digest, same membership as its own header claims. So the
    /// body is also checked against what `state.redb` is bound to, one field at
    /// a time here so that a check quietly dropped from the middle of that list
    /// fails on its own name rather than being covered by a neighbour.
    #[test]
    fn a_snapshot_body_must_belong_to_this_store() {
        let mut state = genesis_state();
        state.model.last_applied = Some(LogId::new(1, LEADER, 5));

        let good = header_for(&state);
        // `good` describes this state at this state's own applied index, so it
        // reaches the equal-index comparison and supplies the digest its own
        // database would produce.
        super::snapshot::check_binding(&good, &[0xaa; 32], &state, || Ok([0xaa; 32]))
            .expect("its own header is accepted");

        // Every mutation below must be refused *before* the equal-index
        // comparison is reached -- either because an identity field differs or
        // because the index does. A closure that panics is what asserts that: a
        // plausible constant would let a dropped identity check fall through to
        // a digest comparison that happens to agree.
        let unreached = || panic!("an identity mismatch must be refused before the digest");

        type Header = crate::coordination::rowstream::SnapshotHeaderV1;
        type Mutation = (&'static str, Box<dyn Fn(&mut Header)>);

        let mutations: Vec<Mutation> = vec![
            ("body format version", Box::new(|h| h.format_version += 1)),
            ("schema version", Box::new(|h| h.schema_version += 1)),
            ("group id", Box::new(|h| h.meta.group_id = vec![0x5a; 32])),
            (
                "genesis hash",
                Box::new(|h| h.meta.genesis_hash = vec![0x5b; 32]),
            ),
            (
                "authority key",
                Box::new(|h| h.meta.authority_public_key = vec![0x5c; 32]),
            ),
            (
                "authority domain",
                Box::new(|h| h.meta.authority_domain = "elsewhere.invalid".into()),
            ),
            (
                "deployment profile",
                Box::new(|h| h.meta.deployment_profile_code += 1),
            ),
            (
                "applied index ahead of the database",
                Box::new(|h| {
                    h.meta.last_applied = Some(crate::coordination::contract::LogIdV1 {
                        term: 1,
                        leader_node_id: LEADER.to_vec(),
                        index: 6,
                    })
                }),
            ),
        ];

        for (what, mutate) in mutations {
            let mut header = good.clone();
            mutate(&mut header);
            let err =
                super::snapshot::check_binding(&header, &[0u8; 32], &state, unreached).unwrap_err();
            assert!(
                err.puts_durability_in_question(),
                "{what}: {err:?} should be fatal"
            );
        }

        // A snapshot *behind* the database is the ordinary case: the state has
        // applied entries since the body was streamed.
        let mut behind = good.clone();
        behind.meta.last_applied = Some(crate::coordination::contract::LogIdV1 {
            term: 1,
            leader_node_id: LEADER.to_vec(),
            index: 4,
        });
        super::snapshot::check_binding(&behind, &[0u8; 32], &state, unreached)
            .expect("behind is not a failure");

        // At an equal applied index the identity fields are exhausted, so the
        // complete log id is required and the body is compared with what this
        // store's own state hashes to.
        let mut equal = good.clone();
        equal.meta.last_applied = Some(crate::coordination::contract::LogIdV1 {
            term: 1,
            leader_node_id: LEADER.to_vec(),
            index: 5,
        });
        super::snapshot::check_binding(&equal, &[0xaa; 32], &state, || Ok([0xaa; 32]))
            .expect("the body this state would produce is accepted");

        let err = super::snapshot::check_binding(&equal, &[0xaa; 32], &state, || Ok([0xbb; 32]))
            .expect_err("a different body at the same log id is not this store's");
        assert!(err.puts_durability_in_question(), "{err:?}");

        let mut wrong_leader = equal.clone();
        wrong_leader.meta.last_applied = Some(crate::coordination::contract::LogIdV1 {
            term: 2,
            leader_node_id: LEADER.to_vec(),
            index: 5,
        });
        let err = super::snapshot::check_binding(&wrong_leader, &[0xaa; 32], &state, unreached)
            .expect_err("a different log id at the same index is two histories");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// The whole `snapshots/` directory of another group, dropped in beside
    /// this one's `state.redb`, is refused at open.
    ///
    /// The end-to-end half of the check above: it proves the binding is on the
    /// path a store actually takes, not merely available to be called.
    #[tokio::test]
    async fn a_foreign_snapshot_directory_is_refused_at_open() {
        let theirs = scratch_dir();
        let mine = scratch_dir();

        {
            let (mut machine, shutdown, drain) = machine(&theirs);
            machine.apply([blank(1)]).await.expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build");
            drop(drain);
            drop(shutdown);
        }

        // A store bound to a different group, and otherwise identical.
        {
            let database = seeded(&mine);
            let mut state = genesis_state();
            state.model.authority.group_id = [0x5a; 32];
            rows::commit(&database, state, &ChangeSet::default()).expect("rebind the group");
        }

        for entry in std::fs::read_dir(theirs.join("snapshots")).expect("read") {
            let entry = entry.expect("entry");
            std::fs::copy(entry.path(), mine.join("snapshots").join(entry.file_name()))
                .expect("plant the foreign snapshot");
        }

        let database = Database::create(mine.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&mine))
            .expect_err("another group's snapshot is not this group's");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// A rebuild repairs a body whose name no longer describes it.
    ///
    /// The digest name is a claim about content, and an orphan left under that
    /// name by an earlier run is a claim nothing checked. Reusing it because the
    /// path exists publishes whatever it happens to contain — so the existing
    /// bytes are hashed, and a body that does not hash to its own name is
    /// replaced by the one that does.
    #[tokio::test]
    async fn a_rebuild_repairs_a_body_whose_name_no_longer_matches_it() {
        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");

        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        let path = body_path(&dir, &built.snapshot_id);
        let intact = std::fs::read(&path).expect("read the published body");

        std::fs::write(&path, b"an orphan from an earlier run").expect("corrupt");

        let again = {
            let mut builder = machine.get_snapshot_builder().await;
            builder
                .build_snapshot()
                .await
                .expect("rebuild")
                .meta
                .clone()
        };
        assert_eq!(again.snapshot_id, built.snapshot_id);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            intact,
            "the body under a digest name must be the body that hashes to it"
        );

        drop(drain);
        drop(shutdown);
        let (mut machine, shutdown, drain) = reopen(&dir);
        machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("and the repaired snapshot opens");
        drop(drain);
        drop(shutdown);
    }

    /// Including an orphan too large to hash.
    ///
    /// The repair above promises to verify an existing body before reusing it,
    /// and the verification is a bounded hash — so the one orphan that could
    /// not be verified was the one over the ceiling, and the build died on the
    /// file it was there to replace. That failure is permanent: nothing can
    /// ever be published under that name again.
    ///
    /// Sparse and Unix-only for the reason given in `snapshot`'s own test:
    /// `ftruncate` declares a length over the ceiling without writing half a
    /// gigabyte, and Windows would allocate it.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_rebuild_repairs_an_orphan_too_large_to_hash() {
        use crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES;

        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");

        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        let path = body_path(&dir, &built.snapshot_id);
        let intact = std::fs::read(&path).expect("read the published body");

        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .expect("open the published body")
            .set_len(MAX_SNAPSHOT_BODY_BYTES + 1)
            .expect("declare a length over the ceiling");

        let again = {
            let mut builder = machine.get_snapshot_builder().await;
            builder
                .build_snapshot()
                .await
                .expect("an oversized orphan must not wedge the build")
                .meta
                .clone()
        };
        assert_eq!(again.snapshot_id, built.snapshot_id);
        assert_eq!(
            std::fs::read(&path).expect("read"),
            intact,
            "the name vouches for its bytes again"
        );

        drop(drain);
        drop(shutdown);
    }

    /// A reader keeps the snapshot it was handed while a newer one is
    /// published.
    ///
    /// Publication retires the predecessor under the same lock that hands out a
    /// lease, so a reader cannot be given a path that is about to be unlinked —
    /// that ordering is carried by `lease`'s signature, which takes the guard.
    /// What this proves is the other half: the pin is real, and it ends.
    #[tokio::test]
    async fn a_reader_keeps_its_snapshot_while_a_newer_one_is_published() {
        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);

        machine.apply([blank(1)]).await.expect("apply");
        {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build");
        }
        let reader = machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("published");
        let first = reader.meta.snapshot_id.clone();
        let first_path = body_path(&dir, &first);
        let first_bytes = body_of(&reader.snapshot);

        machine.apply([blank(2)]).await.expect("apply");
        let second = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        assert_ne!(second.snapshot_id, first, "a newer snapshot was published");
        assert!(
            first_path.exists(),
            "the superseded body is pinned while a reader still holds it"
        );
        assert_eq!(
            body_of(&reader.snapshot),
            first_bytes,
            "and still reads its own body, not the one that replaced it"
        );

        drop(reader);
        assert!(
            !first_path.exists(),
            "and the catalog unlinks it once the last lease ends"
        );
        drop(drain);
        drop(shutdown);
    }

    /// A publication that half-succeeds latches the store fatal.
    ///
    /// `publish_current` can rename `CURRENT` into place and then fail to make
    /// the replacement durable. Returning that error and carrying on was the
    /// defect: the marker on disk had moved, the value held in memory had not,
    /// and the next publication would decide monotonicity from metadata that
    /// was already wrong — which is how a marker regresses without anything
    /// ever reporting corruption.
    #[tokio::test]
    async fn a_publication_that_cannot_be_made_durable_stops_the_store() {
        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");

        super::snapshot::fault::fail_next_directory_fsync();
        let mut builder = machine.get_snapshot_builder().await;
        builder
            .build_snapshot()
            .await
            .expect_err("the publication cannot be made durable");

        assert!(
            machine.handle().is_fatal(),
            "a publication that cannot be made durable leaves the store unable \
             to say what is published"
        );
        assert!(
            machine.get_current_snapshot().await.is_err(),
            "so it must not answer from the metadata it happens to be holding"
        );
        assert!(
            builder.build_snapshot().await.is_err(),
            "nor build another snapshot on top of it"
        );
        assert!(machine.apply([blank(2)]).await.is_err(), "nor apply");
        assert!(machine.applied_state().await.is_err());

        drop(drain);
        drop(shutdown);
    }

    /// A report that cannot open the body it names stops the store too.
    ///
    /// Publication is not the only way the marker and the disk can part
    /// company. `get_current_snapshot` answers from the metadata it holds and
    /// pins the file in the same step, and a pin that fails means the body the
    /// marker names is no longer there — a follower would be told a snapshot
    /// exists and then handed nothing. Returning the error without latching
    /// left the store willing to make that same claim to the next caller.
    #[tokio::test]
    async fn a_current_snapshot_that_cannot_be_pinned_stops_the_store() {
        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");
        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };

        // The body goes from under the marker: a lost file, or a `snapshots/`
        // directory someone tidied.
        std::fs::remove_file(body_path(&dir, &built.snapshot_id)).expect("remove the body");

        machine
            .get_current_snapshot()
            .await
            .expect_err("a snapshot that cannot be pinned cannot be reported");
        assert!(
            machine.handle().is_fatal(),
            "a marker naming a body that is gone is not a transient read error"
        );
        assert!(
            machine.get_current_snapshot().await.is_err(),
            "so the next caller is refused rather than told the snapshot exists"
        );

        drop(drain);
        drop(shutdown);
    }

    /// Two stores in the same group at the same log id, describing different
    /// state.
    ///
    /// Everything the identity checks compare is equal here — group, genesis,
    /// authority, domain, profile, applied index, complete log id — because
    /// both stores really are the same group and really did apply an entry at
    /// index 1. Only the *content* differs, and §11.4's determinism invariant
    /// is what makes that decidable: at one complete `last_applied` the
    /// canonical body is a pure function of replicated state, so a store can
    /// recompute the digest its own database implies and refuse a body that
    /// disagrees.
    #[tokio::test]
    async fn a_same_group_snapshot_of_different_state_is_refused_at_open() {
        let theirs = scratch_dir();
        let mine = scratch_dir();

        // The same log id on both sides: index 1, same term, same leader. One
        // applies a blank, the other a real command.
        {
            let (mut machine, shutdown, drain) = machine(&theirs);
            machine.apply([normal(1, acquire(1))]).await.expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build");
            drop(drain);
            drop(shutdown);
        }
        let published = {
            let (mut machine, shutdown, drain) = machine(&mine);
            machine.apply([blank(1)]).await.expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            let meta = builder.build_snapshot().await.expect("build").meta.clone();
            drop(drain);
            drop(shutdown);
            meta
        };

        // Their whole snapshots directory, marker included, replaces mine. Both
        // sides are internally consistent; the marker names the body, and the
        // body hashes to the name.
        std::fs::remove_file(body_path(&mine, &published.snapshot_id)).expect("clear mine");
        for entry in std::fs::read_dir(theirs.join("snapshots")).expect("read") {
            let entry = entry.expect("entry");
            std::fs::copy(entry.path(), mine.join("snapshots").join(entry.file_name()))
                .expect("plant it");
        }

        let database = Database::create(mine.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&mine))
            .expect_err("a body describing state this voter never had");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// A complete, valid body that is not the body its name claims.
    ///
    /// The one corruption the metadata checks cannot catch on their own: two
    /// stores at the same log id with different state produce metadata that
    /// agrees field for field and bodies that differ. Only the digest separates
    /// them, which is why the digest is what the file is named after — and why
    /// this test builds the second body from a genuinely different group rather
    /// than by flipping bytes, which any of the other checks would also catch.
    #[tokio::test]
    async fn a_body_that_is_not_the_body_its_name_claims_refuses_the_open() {
        let mine = scratch_dir();
        let theirs = scratch_dir();

        let published = {
            let (mut machine, shutdown, _drain) = machine(&mine);
            machine.apply([blank(1)]).await.expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            let meta = builder.build_snapshot().await.expect("build").meta.clone();
            drop(shutdown);
            meta
        };

        let (other_meta, other_bytes) = {
            let (mut machine, shutdown, _drain) = machine(&theirs);
            // The same log id, different state: an acquire at index 1 rather
            // than a blank.
            machine.apply([normal(1, acquire(1))]).await.expect("apply");
            let mut builder = machine.get_snapshot_builder().await;
            let snapshot = builder.build_snapshot().await.expect("build");
            let bytes = body_of(&snapshot.snapshot);
            let meta = snapshot.meta.clone();
            drop(snapshot);
            drop(shutdown);
            (meta, bytes)
        };

        assert_eq!(
            other_meta.last_log_id, published.last_log_id,
            "the two bodies describe the same log entry"
        );
        assert_eq!(other_meta.last_membership, published.last_membership);
        assert_ne!(
            other_meta.snapshot_id, published.snapshot_id,
            "and differ only in content, which is the case under test"
        );

        std::fs::write(body_path(&mine, &published.snapshot_id), other_bytes)
            .expect("substitute the body");

        let database = Database::create(mine.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&mine))
            .expect_err("a body that does not hash to its name is refused");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// Rebuilding the identical snapshot republishes the marker and nothing
    /// else — the repair path after a lost acknowledgement.
    #[tokio::test]
    async fn rebuilding_the_same_snapshot_repairs_the_marker_idempotently() {
        let dir = scratch_dir();
        let layout = StorageLayout::new(&dir);
        let (mut machine, shutdown, drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");

        let first = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        // The marker is lost, which is what a crash between the state
        // replacement and the marker publication leaves behind.
        std::fs::remove_file(layout.current_snapshot_marker()).expect("drop the marker");

        let second = {
            let mut builder = machine.get_snapshot_builder().await;
            builder
                .build_snapshot()
                .await
                .expect("rebuild")
                .meta
                .clone()
        };
        assert_eq!(second, first, "the same state produces the same snapshot");

        drop(drain);
        drop(shutdown);
        assert_eq!(
            reload_current(&layout, &dir)
                .expect("load")
                .expect("republished")
                .meta,
            crate::coordination::raft::SnapshotMetaV1::from(&first),
            "the marker is repaired without the body being rewritten"
        );
    }

    /// A marker naming a body that is not there is data loss, not "no
    /// snapshot".
    #[tokio::test]
    async fn a_marker_whose_body_is_missing_refuses_the_open() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");
        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        drop(shutdown);

        std::fs::remove_file(body_path(&dir, &built.snapshot_id)).expect("remove the body");

        let database = Database::create(dir.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&dir))
            .expect_err("a marker without its body is refused");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// A body that is truncated no longer hashes to the name it is under.
    #[tokio::test]
    async fn a_truncated_body_refuses_the_open() {
        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");
        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        drop(shutdown);

        let path = body_path(&dir, &built.snapshot_id);
        let bytes = std::fs::read(&path).expect("read");
        std::fs::write(&path, &bytes[..bytes.len() - 1]).expect("truncate");

        let database = Database::create(dir.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&dir))
            .expect_err("a truncated body is refused");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }

    /// A marker whose metadata disagrees with its body is data loss, even
    /// though both the marker and the body are individually well formed.
    ///
    /// This is the check that stops the marker being believed on its own. It is
    /// the same rule install applies to the `SnapshotMeta` OpenRaft supplies,
    /// so getting it wrong here would mean getting it wrong there.
    #[tokio::test]
    async fn a_marker_that_disagrees_with_its_body_refuses_the_open() {
        let dir = scratch_dir();
        let layout = StorageLayout::new(&dir);
        let (mut machine, shutdown, _drain) = machine(&dir);
        machine.apply([blank(1)]).await.expect("apply");
        let built = {
            let mut builder = machine.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build").meta.clone()
        };
        drop(shutdown);

        // The same body, under the same id, described as a different entry.
        let mut lying = crate::coordination::raft::SnapshotMetaV1::from(&built);
        lying.last_log_id = lying
            .last_log_id
            .map(|id| crate::coordination::contract::LogIdV1 {
                index: id.index + 1,
                ..id
            });
        let bytes = super::super::codec::encode_committed(
            Db::State,
            "SnapshotMetaV1",
            &lying,
            super::super::schema::MAX_COORDINATION_META_BYTES,
        )
        .expect("encode");
        std::fs::write(layout.current_snapshot_marker(), bytes).expect("rewrite the marker");

        let database = Database::create(dir.join("state.redb")).expect("reopen");
        let err = CoordinationStateMachine::new(database, StorageLayout::new(&dir))
            .expect_err("a marker that does not describe its body is refused");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }
}
