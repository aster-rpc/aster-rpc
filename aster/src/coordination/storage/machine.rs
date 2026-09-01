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
use tokio::sync::{mpsc, oneshot, watch};

use super::actor::ActorShutdown;
use super::error::{Db, StorageError, StorageResult};
use super::reads::{GuardedRead, Quiesced, ReadDrain, ReadGate, ReadLease};
use super::rows::{self, DurableState};
use super::schema::StorageLayout;
use super::snapshot::{self, SnapshotCatalog, SnapshotFile};
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

use crate::coordination::model::state::{Intent, OperationId};

pub(crate) type Entry = openraft::Entry<CoordinationTypeConfig>;
type Reply<T> = oneshot::Sender<StorageResult<T>>;

/// One `Blank` entry at `index`, attributed to `leader`.
///
/// Module-level rather than inside the test module because the S5-E snapshot
/// benchmark needs it too: it advances a sender's applied index without
/// touching a row, so the body it then streams is the fixture's shape and not
/// one the harness quietly altered. The test module's own `blank` delegates
/// here, so there is one definition of what a blank entry is.
#[cfg(test)]
pub(crate) fn blank_entry(leader: [u8; 32], index: u64) -> Entry {
    Entry {
        log_id: RaftLogId {
            leader_id: openraft::CommittedLeaderId::new(1, CoordinationNodeId::from_array(leader)),
            index,
        },
        payload: EntryPayload::Blank,
    }
}

/// A reply whose failure is **not** a `StorageError`.
///
/// Preparation is the one command whose two failures must stay apart: a
/// refusal of the sender's bytes and this store's own staging failure are
/// answered to different audiences, so flattening them into `StorageResult`
/// here would lose the distinction before the caller ever sees it.
type PreparationReply =
    oneshot::Sender<std::result::Result<snapshot::PreparedSnapshot, snapshot::PrepareInstallError>>;

fn admin_view_of(state: &DurableState) -> AdminView {
    AdminView {
        revision: state.model.admin_revision,
        root: state.model.authority.public_key,
        delegates: state.model.admins.clone(),
    }
}

/// The §8.1 administrative facts, republished on every apply and install so
/// the Store service can make its **cheap preliminary** authorization check
/// without an actor round trip or a barrier. Advisory only: the authoritative
/// check stays inside the guarded read transaction. Safe as a pre-denial on
/// the leader because `client_write` returns only after apply — an appointed
/// administrator is in this view before it can learn of its appointment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdminView {
    pub(crate) revision: u64,
    pub(crate) root: [u8; 32],
    pub(crate) delegates: std::collections::BTreeSet<[u8; 32]>,
}

impl AdminView {
    pub(crate) fn is_administrator(&self, actor: &[u8; 32]) -> bool {
        self.revision > 0 && (self.root == *actor || self.delegates.contains(actor))
    }
}

/// What [`StateCommand::Preflight`] answers.
#[derive(Debug)]
pub(crate) enum PreflightAnswer {
    /// The §10 stage the request lands on: resolved, denied with a reason,
    /// denied with a status, or admitted for proposal.
    Admission(crate::coordination::model::apply::Admission),
    /// The classifier left the sequence unresolved and the action never
    /// narrowed: the handler answers its stage-4 `INVALID_ARGUMENT`.
    ShapelessUnresolved,
}

// ── Actor protocol ───────────────────────────────────────────────────────────

/// The §14 facts only the state machine holds (see
/// [`StateCommand::Observability`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObservabilityFacts {
    /// The group's genesis hash, from the bound authority.
    pub(crate) genesis_hash: [u8; 32],
    /// Whether the committed genesis has been applied.
    ///
    /// `admin_revision > 0` for an ordinary store; an applied
    /// `OPERATION_GENESIS` audit row for a §13-recovered one, whose import
    /// writes a nonzero revision before any log exists. See
    /// [`genesis_applied_fact`].
    pub(crate) genesis_installed: bool,
    /// The lowest retained audit row's log index; `None` before any pruning.
    pub(crate) audit_floor_index: Option<u64>,
}

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
    /// Has the committed genesis been applied? (§7.6 readiness.)
    GenesisInstalled(Reply<bool>),
    /// The bounded §14 state-machine facts: the group's genesis hash,
    /// whether it is installed, and the audit floor. Identifiers and
    /// indexes only — no row data leaves the actor.
    Observability(Reply<ObservabilityFacts>),
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
    /// The one active (non-terminal) membership intent, if any.
    ///
    /// Targeted rather than a whole-state copy: the §7.3 controller re-reads
    /// this before every side effect, and copying a hundred thousand resource
    /// rows to find one intent would make the correct behaviour — re-observing
    /// rather than remembering — the expensive one.
    ActiveIntent(Reply<Option<(OperationId, Intent)>>),
    /// §8.2.1/§10 stages 2–4 for one sequenced mutation, on the same state
    /// apply runs against.
    ///
    /// On the actor deliberately: preflight and apply share one decision
    /// function, and running preflight anywhere else would mean either copying
    /// the model per request or rebuilding it from a transaction — both of
    /// which are how two implementations of one decision start to drift. The
    /// applied log id returns with the answer so the Store handler can prove
    /// the state consulted was at or past its linearizable barrier.
    ///
    /// `action` is `None` when the decoded action failed shape narrowing:
    /// session resolution still runs (§10 stage 0 is *decode* failure, not
    /// bounds failure), and an unresolved sequence then takes the stage-4
    /// status in the handler.
    Preflight {
        client: Box<crate::coordination::model::apply::ClientRef>,
        action: Option<Box<crate::coordination::model::apply::Action>>,
        reply: Reply<(PreflightAnswer, Option<LogId>, bool)>,
    },
    /// The leader-side decision for one sessionless `retire_client` (§10), on
    /// the same state apply runs against, with the same applied witness.
    RetirePreflight {
        command: crate::coordination::model::retire::RetireCommand,
        reply: Reply<(crate::coordination::model::retire::Decision, Option<LogId>)>,
    },
    /// Install an already-prepared snapshot over this store's state.
    ///
    /// Takes the proved role only. `supplied` is OpenRaft's own copy of the
    /// metadata, compared field for field against the metadata sealed inside
    /// `prepared` before anything is replaced.
    Install {
        prepared: Box<snapshot::PreparedSnapshot>,
        supplied: crate::coordination::raft::SnapshotMetaV1,
        reply: Reply<()>,
    },
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
    /// `Option` because install closes it: `state.redb` cannot be renamed over
    /// while redb still holds it, and the reopen that follows is what proves
    /// the replacement landed. Every other path expects it present.
    database: Option<Database>,
    /// Where this store's files are, for the install that runs here.
    layout: StorageLayout,
    /// Shared with the machine, so a snapshot published by an install and one
    /// published by a build go through the same catalog.
    catalog: SnapshotCatalog,
    /// The publication critical section, shared with the machine.
    ///
    /// Taken **after** [`StateActor::reads`] is quiesced, never before. A
    /// builder holds its read lease across `publish`, which takes this mutex;
    /// an install that took the mutex first and then waited for the lease would
    /// be waiting for a builder that is waiting for it.
    current: Arc<Mutex<Option<SnapshotFile>>>,
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
    /// The **committed** membership, published for readers outside this actor.
    ///
    /// This actor is the only component in the process that sees committed
    /// membership, because it is the only one that applies committed entries.
    /// `RaftMetrics::membership_config` is `membership_state.effective()` —
    /// the newest membership *seen*, committed or not — and six call sites
    /// read it while their own documentation said "committed". Publishing from
    /// here means a reader gets the authoritative value without a round trip
    /// and without a second opinion that disagrees exactly while a change is in
    /// flight.
    ///
    /// Written only where [`Self::state`] is, through
    /// [`Self::install_state`], so the two cannot diverge.
    committed: watch::Sender<Arc<StoredMembership<CoordinationNodeId, CoordinationNode>>>,
    /// The §8.1 administrative view, republished with every install.
    admins: watch::Sender<Arc<AdminView>>,
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
    /// Publish a new durable state, and the committed membership inside it.
    ///
    /// The **only** way [`Self::state`] is assigned. Both places that replace
    /// the state — an applied batch and an installed snapshot — change the
    /// committed membership, and a publish that lived at only one of them would
    /// leave every reader believing a group's membership was whatever it was
    /// before the snapshot landed.
    /// Takes the two fields rather than `&mut self` so it can be called from
    /// the install path, which is holding the current-snapshot mutex — a
    /// whole-`self` borrow there would not compile, and splitting the publish
    /// back out to the call sites is exactly the divergence this prevents.
    fn install_state(
        slot: &mut Option<DurableState>,
        committed: &watch::Sender<Arc<StoredMembership<CoordinationNodeId, CoordinationNode>>>,
        admins: &watch::Sender<Arc<AdminView>>,
        state: DurableState,
    ) {
        // `send_replace` rather than `send`: there may be no receiver yet, or
        // ever, and a membership that failed to publish because nobody was
        // listening is exactly the silent divergence this exists to prevent.
        committed.send_replace(Arc::new(state.membership.clone()));
        admins.send_replace(Arc::new(admin_view_of(&state)));
        *slot = Some(state);
    }

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
                // Not through `serve`: preparation's failure type is not
                // `StorageError`, precisely so a refusal of the sender's bytes
                // cannot be mistaken for this store's own failure and latch it
                // fatal. `serve` exists to do that latching.
                StateCommand::GenesisInstalled(reply) => {
                    self.serve(reply, StateActor::genesis_installed)
                }
                StateCommand::Observability(reply) => self.serve(reply, StateActor::observability),
                StateCommand::ActiveIntent(reply) => self.serve(reply, StateActor::active_intent),
                StateCommand::Preflight {
                    client,
                    action,
                    reply,
                } => self.serve(reply, |a| a.preflight(&client, action.as_deref())),
                StateCommand::RetirePreflight { command, reply } => {
                    self.serve(reply, |a| a.retire_preflight(&command))
                }
                StateCommand::Install {
                    prepared,
                    supplied,
                    reply,
                } => self.serve(reply, |a| a.install(*prepared, &supplied)),
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
                StateCommand::GenesisInstalled(reply) => fail(reply),
                StateCommand::Observability(reply) => fail(reply),
                StateCommand::ActiveIntent(reply) => fail(reply),
                StateCommand::Preflight { reply, .. } => fail(reply),
                StateCommand::RetirePreflight { reply, .. } => fail(reply),
                StateCommand::Install { reply, .. } => fail(reply),
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

    /// The open `state.redb`.
    ///
    /// Absent only while install has closed it to rename over it, which is a
    /// window no command can be serviced inside: install holds the actor for
    /// its whole duration.
    fn database(&self) -> &Database {
        self.database
            .as_ref()
            .expect("state.redb is open outside an install's replacement window")
    }

    /// Install a snapshot over this store's state (§11.4).
    ///
    /// Runs here, on the actor, because only the actor can close `state.redb`
    /// before the rename and reopen it after — and because the classification
    /// and the rename must see one `last_applied`. An apply landing between
    /// them turns an accepted higher snapshot into a rollback, and the actor
    /// servicing one command at a time is what makes that structural rather
    /// than timed.
    ///
    /// ## The two locks, in this order
    ///
    /// Quiescence first, the publication mutex second. A snapshot builder holds
    /// its read lease across `publish`, which takes that mutex; an install that
    /// took the mutex first and then waited out the leases would be waiting for
    /// a builder that is waiting for it. Neither would ever finish, and neither
    /// is doing anything wrong.
    ///
    /// Both are then held through everything that follows — replacement,
    /// reopen, revalidation, body promotion, `CURRENT`, retirement, and the
    /// in-memory update — because a store whose database and marker disagree is
    /// exactly what a reader must never be admitted to.
    /// The one active membership intent, read from the published state.
    ///
    /// `None` means there is none — which is not "we finished": that is a fact
    /// the intent's own terminal phase records, and it is exactly the
    /// distinction a controller must not blur.
    /// Whether the committed genesis has been applied.
    ///
    /// For an ordinary store, `admin_revision > 0` is the fact, not a proxy
    /// for it: the only thing that sets it to 1 is `apply_genesis` installing
    /// the delegated set, and `set_admin` cannot run before that because it
    /// needs an administrator. So a nonzero revision means the genesis entry
    /// is in this node's applied state, whether by replay or snapshot.
    ///
    /// A §13-recovered store breaks that equivalence on purpose: the import
    /// writes the administrator union and `source_admin_revision + 1` *before*
    /// any log exists, and the plan still requires the signed Genesis command
    /// to commit and confirm the import. So for a recovered store the fact is
    /// an applied `OPERATION_GENESIS` audit row. Deterministic and replicated;
    /// and if audit pruning ever evicts that row in a very old group, the
    /// leader merely re-proposes the same genesis, whose apply is the
    /// documented idempotent no-op that writes a fresh row — convergent, never
    /// fatal.
    fn genesis_installed(&mut self) -> StorageResult<bool> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok(genesis_applied_fact(state))
    }

    fn observability(&mut self) -> StorageResult<ObservabilityFacts> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok(ObservabilityFacts {
            genesis_hash: state.model.authority.genesis_hash,
            genesis_installed: genesis_applied_fact(state),
            audit_floor_index: state.model.audit_floor.as_ref().map(|id| id.index),
        })
    }

    fn active_intent(&mut self) -> StorageResult<Option<(OperationId, Intent)>> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok(state
            .model
            .intents
            .iter()
            .find(|(_, intent)| {
                !crate::coordination::model::membership::is_terminal_phase(intent.status_code)
            })
            .map(|(id, intent)| (*id, intent.clone())))
    }

    /// Resolve a prepared snapshot against this store's state and act on the
    /// verdict — the installation half.
    ///
    /// Accepts **only** a value preparation produced. A raw role reaching here
    /// in production is a fatal invariant rather than a fallback: the type
    /// carries the proof, so accepting an unproved one would silently reinstate
    /// the boundary this split exists to close.
    fn install(
        &mut self,
        prepared: snapshot::PreparedSnapshot,
        supplied: &crate::coordination::raft::SnapshotMetaV1,
    ) -> StorageResult<()> {
        // The token that owns the vote, the metadata and the body cannot reach
        // across OpenRaft's call — `install_snapshot` receives `meta` as its own
        // argument — so the binding is re-established here, field for field,
        // against the metadata sealed inside the prepared value.
        //
        // A mismatch is a **fatal invariant**, not a rejection: by this point
        // both values are ours. The sender's copy was already checked against
        // the body during preparation.
        if prepared.meta() != supplied {
            return Err(StorageError::corrupt(
                Db::State,
                "snapshot",
                format!(
                    "OpenRaft supplied metadata {supplied:?} for a body prepared against {:?}; \
                     two valid preparations were crossed",
                    prepared.meta()
                ),
            ));
        }
        let prepared = prepared.into_prepared();

        let _exclusive = self
            .reads
            .quiesce()
            .ok_or(StorageError::ActorStopped { db: Db::State })?;
        let mut current = self.current.lock().expect("the current-snapshot mutex");

        let resolved = {
            let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
            prepared.resolve(state, || snapshot::canonical_digest(self.database()))?
        };

        let body = match resolved {
            // Not the benign race it reads as: OpenRaft filters these out
            // before calling the state machine, so reaching here means its
            // view and the durable state have already diverged.
            snapshot::ResolvedInstall::Stale(detail) => {
                return Err(StorageError::SnapshotBehind { detail })
            }
            // This voter's history and the group's disagree.
            snapshot::ResolvedInstall::Fatal(detail) => {
                return Err(StorageError::corrupt(Db::State, "snapshot", detail))
            }
            // The business state is already correct: only the marker is owed.
            snapshot::ResolvedInstall::Repair(repair) => repair.publishable(),
            snapshot::ResolvedInstall::Replace(granted) => {
                // Closed before the rename, reopened after: redb holds the file
                // this is about to be renamed over.
                drop(self.database.take());
                let replaced = granted.replace()?;

                // Service does not resume on the replacement alone, and the
                // publication capability is not reachable until it has: the
                // reopen proves the file that landed is a database this build
                // can serve *and* is the one this install validated.
                let (database, reloaded, body) = replaced.reopen()?;
                self.database = Some(database);
                Self::install_state(&mut self.state, &self.committed, &self.admins, reloaded);
                body
            }
        };

        let published = snapshot::publish_installed(&self.layout, &self.catalog, body)?;
        *current = Some(published);
        Ok(())
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
            .read(self.database())
            .map_err(|source| StorageError::redb(Db::State, source))
    }

    fn applied_state(&mut self) -> StorageResult<Applied> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok((
            state.model.last_applied.as_ref().map(Into::into),
            state.membership.clone(),
        ))
    }

    /// §10 stages 2–4 for the Store leader, on exactly the state apply runs
    /// against, with the applied log id as the barrier witness. The third
    /// element reports whether an admitted request would **create** a session
    /// row — the §11.5 "new clients" fact the pressure gate needs, taken from
    /// the same classifier apply uses rather than re-derived at the service.
    fn preflight(
        &mut self,
        client: &crate::coordination::model::apply::ClientRef,
        action: Option<&crate::coordination::model::apply::Action>,
    ) -> StorageResult<(PreflightAnswer, Option<LogId>, bool)> {
        use crate::coordination::model::apply::{preflight, preflight_shapeless};
        use crate::coordination::model::classify::{classify, Classification};
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        let applied = state.model.last_applied;
        let session = state.model.sessions.get(&(client.actor, client.client_id));
        let capacity = state.model.capacity_for(&client.actor);
        let creates_session = matches!(
            classify(session, client.sequence, &client.request_hash, capacity),
            Classification::Unresolved {
                creates_session: true,
                ..
            }
        );
        let answer = match action {
            Some(action) => PreflightAnswer::Admission(preflight(&state.model, client, action)),
            None => match preflight_shapeless(&state.model, client) {
                Some(admission) => PreflightAnswer::Admission(admission),
                None => PreflightAnswer::ShapelessUnresolved,
            },
        };
        Ok((answer, applied, creates_session))
    }

    /// The `retire_client` decision (§10), on the applied state, with the same
    /// witness.
    fn retire_preflight(
        &mut self,
        command: &crate::coordination::model::retire::RetireCommand,
    ) -> StorageResult<(crate::coordination::model::retire::Decision, Option<LogId>)> {
        let state = self.state.as_ref().ok_or(StorageError::Fatal)?;
        Ok((
            crate::coordination::model::retire::decide(&state.model, command),
            state.model.last_applied,
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
                    // §8.1: ROOT is never a committed member. Enforced here as
                    // well as at admission, and from the same predicate the
                    // loader uses — admission checks do not see a forced native
                    // entry from a non-conforming leader, and a rule that lives
                    // only there is a rule a store can be made to violate.
                    if rows::membership_names_root(membership, &working.model.authority.public_key)
                    {
                        return Err(StorageError::corrupt(
                            Db::State,
                            "membership",
                            "a membership entry names the deployment ROOT; ROOT is never a \
                             committed voter or learner",
                        ));
                    }
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
        let committed = rows::commit(self.database(), working, &changes)?;
        Self::install_state(&mut self.state, &self.committed, &self.admins, committed);
        // The batch is durable and its replies exist. Dying here is the lost
        // acknowledgement: the entry applied, and the caller never learned it.
        // What must not follow is the entry being applied a second time, which
        // is the §10 session table's job and what the matrix checks.
        super::crash::reached(super::crash::CrashPoint::Acknowledgement);
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

/// Reclassify a validation failure as a refusal of somebody else's bytes.
///
/// Only the codec's verdict moves. `StoredCorruption` means "this build cannot
/// read what is on disk", which is fatal when the disk is ours and merely a bad
/// message when the bytes arrived from a peer -- and install is the one place
/// that distinction has to be made, because it is the one place the two kinds
/// of bytes meet. Everything else keeps its classification: a failed fsync
/// while staging is this store's failure no matter what provoked it.
fn rejected(err: StorageError) -> StorageError {
    match err {
        StorageError::StoredCorruption { db, what, detail } => StorageError::SnapshotRejected {
            detail: format!("{db} {what}: {detail}"),
        },
        // Same reasoning as the codec's verdict: the bytes are the sender's.
        // Only `receive` raises this -- the build path's ceiling arrives as a
        // plain write failure, which is fatal and correct but not this variant.
        too_large @ StorageError::SnapshotTooLarge { .. } => StorageError::SnapshotRejected {
            detail: too_large.to_string(),
        },
        other => other,
    }
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
    /// Where this store's files live. Carried so preparation can run without
    /// asking the actor for anything.
    layout: StorageLayout,
    /// The immutable bindings a snapshot is checked against. Read once, at
    /// spawn: every field is fixed when storage opens, so a copy here cannot
    /// go stale — which is exactly why preparation does not need the actor.
    identity: Arc<snapshot::SnapshotIdentity>,
    /// The committed membership, republished by the actor on every apply and
    /// every snapshot install.
    ///
    /// A `watch` rather than a command: the readers are the sender allowlist,
    /// the lifecycle role and the membership controller, and the first of those
    /// is consulted on **every inbound Raft request**. A round trip to the
    /// actor there would queue admission behind whatever the actor is applying,
    /// which is the wrong thing to make slower and the wrong thing to couple.
    committed: watch::Receiver<Arc<StoredMembership<CoordinationNodeId, CoordinationNode>>>,
    /// The §8.1 administrative facts, for the Store service's cheap
    /// preliminary check — same discipline as `committed`.
    admins: watch::Receiver<Arc<AdminView>>,
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn spawn(
        database: Database,
        state: DurableState,
        reads: ReadGate,
        layout: StorageLayout,
        catalog: SnapshotCatalog,
        current: Arc<Mutex<Option<SnapshotFile>>>,
    ) -> StorageResult<(Self, StateShutdown)> {
        // Read before the state moves into the actor. Both are immutable for
        // the life of the store, which is what lets preparation run off the
        // actor entirely.
        let handle_identity = snapshot::SnapshotIdentity::of(&state);
        let handle_layout = layout.clone();
        let (tx, rx) = mpsc::channel(STATE_ACTOR_QUEUE);
        let stop = Arc::new(AtomicBool::new(false));
        let actor_stop = Arc::clone(&stop);
        let fatal = Arc::new(AtomicBool::new(false));
        let actor_fatal = Arc::clone(&fatal);
        // Seeded from the state that was just loaded and validated, so a reader
        // that asks before the first apply gets the membership this store
        // reopened with rather than an empty one.
        let (committed_tx, committed_rx) = watch::channel(Arc::new(state.membership.clone()));
        let (admins_tx, admins_rx) = watch::channel(Arc::new(admin_view_of(&state)));
        let thread = std::thread::Builder::new()
            .name("aster-coordination-state".into())
            .spawn(move || {
                StateActor {
                    database: Some(database),
                    reads,
                    layout,
                    catalog,
                    current,
                    state: Some(state),
                    committed: committed_tx,
                    admins: admins_tx,
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
                layout: handle_layout,
                identity: Arc::new(handle_identity),
                committed: committed_rx,
                admins: admins_rx,
            },
            ActorShutdown::new(tx, stop, thread, || StateCommand::Shutdown),
        ))
    }

    pub(crate) fn is_fatal(&self) -> bool {
        self.fatal.load(Ordering::SeqCst)
    }

    /// The **committed** membership: exactly the entries this store has
    /// applied.
    ///
    /// The value `RaftMetrics::membership_config` is not. OpenRaft populates
    /// that from `membership_state.effective()` — the newest membership entry
    /// seen, committed or not — and a node named only by an uncommitted entry
    /// must not graduate off its signed seed allowlist, be reported as a voter,
    /// or satisfy a completion check, because that entry can still be rolled
    /// back by a new leader.
    ///
    /// Cheap: a `watch` borrow and an `Arc` clone, with no actor round trip.
    pub(crate) fn committed_membership(
        &self,
    ) -> Arc<StoredMembership<CoordinationNodeId, CoordinationNode>> {
        Arc::clone(&self.committed.borrow())
    }

    /// The advisory §8.1 administrative view — cheap, no actor round trip.
    pub(crate) fn admin_view(&self) -> Arc<AdminView> {
        Arc::clone(&self.admins.borrow())
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

    /// The one active membership intent, for the §7.3 controller.
    /// Whether the committed genesis has been applied (§7.6).
    pub(crate) async fn genesis_installed(&self) -> StorageResult<bool> {
        self.call(StateCommand::GenesisInstalled).await
    }

    /// The bounded §14 state-machine facts, for the local handle's
    /// consensus snapshot.
    pub(crate) async fn observability(&self) -> StorageResult<ObservabilityFacts> {
        self.call(StateCommand::Observability).await
    }

    /// The one active membership intent, for the §7.3 controller.
    pub(crate) async fn active_intent(&self) -> StorageResult<Option<(OperationId, Intent)>> {
        self.call(StateCommand::ActiveIntent).await
    }

    /// A snapshot-consistent read transaction over `state.redb`.
    pub(crate) async fn read_transaction(&self) -> StorageResult<GuardedRead> {
        self.call(StateCommand::ReadTransaction).await
    }

    /// §8.2.1/§10 stages 2–4 for one sequenced mutation, with the applied
    /// witness (see [`StateCommand::Preflight`]).
    pub(crate) async fn preflight(
        &self,
        client: crate::coordination::model::apply::ClientRef,
        action: Option<crate::coordination::model::apply::Action>,
    ) -> StorageResult<(PreflightAnswer, Option<LogId>, bool)> {
        self.call(|reply| StateCommand::Preflight {
            client: Box::new(client),
            action: action.map(Box::new),
            reply,
        })
        .await
    }

    /// The leader-side `retire_client` decision, with the applied witness.
    pub(crate) async fn retire_preflight(
        &self,
        command: crate::coordination::model::retire::RetireCommand,
    ) -> StorageResult<(crate::coordination::model::retire::Decision, Option<LogId>)> {
        self.call(|reply| StateCommand::RetirePreflight { command, reply })
            .await
    }

    /// Stream, verify and rebuild a raw snapshot source (§11.4 preparation).
    ///
    /// **This is the whole install boundary.** It runs outside any OpenRaft
    /// call, so a refusal of the sender's bytes is answered by the caller and
    /// the voter keeps serving — where the same refusal inside
    /// `install_snapshot` becomes an OpenRaft `Fatal` that ends the
    /// state-machine worker.
    ///
    /// ## Off the actor, on a blocking worker
    ///
    /// An earlier revision ran this *on* the state actor, which put a rebuild
    /// of up to 512 MiB on the single-writer thread that also serves applies
    /// and reads — a snapshot transfer stalled unrelated work for its whole
    /// duration. It does not need to be there: everything preparation compares
    /// against is a [`SnapshotIdentity`](snapshot::SnapshotIdentity), and every
    /// field of one is fixed when storage opens and immutable for the group's
    /// lifetime. The comparison that *does* need the live state — the
    /// candidate's applied index against this store's — stays on the actor
    /// under the same exclusion as the rename, which is the split's whole
    /// point.
    ///
    /// `spawn_blocking` rather than the async runtime's worker pool: this is
    /// hundreds of megabytes of file I/O and redb writes, and running it on a
    /// cooperative executor thread starves every other task on it.
    ///
    /// Its own body rather than [`call`](Self::call), because its failure type
    /// is not `StorageError`: preparation's two arms are answered to different
    /// audiences, and a shared helper would have to flatten them. The refusals
    /// it manufactures locally are `Storage`, not `Rejected` — a store that has
    /// stopped says nothing about the sender's snapshot.
    pub(crate) async fn prepare_install(
        &self,
        source: Box<CoordinationSnapshotFile>,
        supplied: crate::coordination::raft::SnapshotMetaV1,
    ) -> std::result::Result<snapshot::PreparedSnapshot, snapshot::PrepareInstallError> {
        if self.is_fatal() {
            return Err(snapshot::PrepareInstallError::Storage(StorageError::Fatal));
        }
        let layout = self.layout.clone();
        let identity = self.identity.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            snapshot::prepare_install(&layout, &source, &identity, &supplied)
        })
        .await;

        match outcome {
            Ok(result) => {
                // A *storage* failure during preparation latches, for the same
                // reason every other one does: this store's disk failed. A
                // rejection does not — that is the property the whole boundary
                // exists for.
                if let Err(snapshot::PrepareInstallError::Storage(err)) = &result {
                    if err.puts_durability_in_question() {
                        self.mark_fatal();
                    }
                }
                result
            }
            // The worker panicked or the runtime is shutting down. Ours either
            // way, and never reported to the peer as a bad snapshot.
            Err(join) => Err(snapshot::PrepareInstallError::Storage(StorageError::io(
                "preparing a snapshot",
                std::path::Path::new("<worker>"),
                std::io::Error::other(join.to_string()),
            ))),
        }
    }

    /// A copy of the published state, for tests that compare memory with disk.    /// A copy of the published state, for tests that compare memory with disk.
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
        let catalog = SnapshotCatalog::new();
        // After `CURRENT` has been validated and before anything can publish or
        // read: the marker is what says which body to keep, and the catalog is
        // empty, so a retirement here unlinks at once.
        snapshot::sweep_orphans(&layout, &catalog, current.as_ref())?;
        let current = Arc::new(Mutex::new(current));
        // The actor gets its own handles on the layout, the catalog and the
        // current marker because install runs *inside* it: only the actor can
        // close `state.redb` before the rename and reopen it after, and §11.4
        // requires the publication that follows to happen without releasing
        // exclusion.
        let (handle, shutdown) = StateHandle::spawn(
            database,
            state,
            reads.clone(),
            layout.clone(),
            catalog.clone(),
            Arc::clone(&current),
        )?;
        Ok((
            Self {
                handle,
                layout,
                catalog,
                current,
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
        // The decision and the marker write live together in `snapshot`, so
        // this module has no way to publish a marker at all -- which is what
        // stops a local build reaching `publish_current` directly and skipping
        // §11.4's monotonicity rule that the install path is held to.
        let serving = snapshot::publish_built(&self.layout, &self.catalog, current, built)?;
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
            meta: rows::snapshot_meta_from_wire(file.meta())?,
            snapshot: Box::new(self.catalog.lease(&file.meta().snapshot_id, file.path())?),
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

    /// Install a snapshot OpenRaft has accepted a vote for.
    ///
    /// Accepts **only** [`CoordinationSnapshotFile::Prepared`]. A raw
    /// `Incoming` or `Published` here is a fatal invariant, not a fallback: the
    /// type carries the proof, and accepting an unproved role would silently
    /// reinstate the boundary the split exists to close. There is deliberately
    /// **no** `cfg(test)` arm — see `SuiteStateMachine` in the tests below for
    /// why an arm here rather than an adapter around this type would make the
    /// real-Raft boundary test prove a property the shipped build lacks.
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>,
        snapshot: Box<CoordinationSnapshotFile>,
    ) -> Result<(), RaftStorageError<CoordinationNodeId>> {
        let supplied = crate::coordination::raft::SnapshotMetaV1::from(meta);
        let prepared = match *snapshot {
            CoordinationSnapshotFile::Prepared(prepared) => *prepared,
            raw => {
                return Err(io_state(StorageError::corrupt(
                    Db::State,
                    "snapshot",
                    format!(
                        "install_snapshot was handed a raw {} role; a snapshot must be proved by \
                         prepare_install outside OpenRaft before it reaches this boundary",
                        raw.role()
                    ),
                )))
            }
        };
        self.handle
            .call(|reply| StateCommand::Install {
                prepared: Box::new(prepared),
                supplied,
                reply,
            })
            .await
            .map_err(io_state)
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<CoordinationTypeConfig>>, RaftStorageError<CoordinationNodeId>>
    {
        self.current_snapshot().map_err(io_read_state)
    }
}

// ── The test-only suite adapter ──────────────────────────────────────────────

/// Run the **production** preparation in front of the **production** install.
///
/// Whether this state's committed genesis command has applied (§7.2, §13).
///
/// Ordinary store: `admin_revision > 0` — only `apply_genesis` moves it off
/// zero. §13-recovered store: the import pre-writes the administrator union
/// and `source_admin_revision + 1` before any log exists, so the revision can
/// no longer answer; the fact becomes an applied `OPERATION_GENESIS` audit
/// row, which the confirming Genesis writes on either of its branches. Both
/// readings are pure functions of replicated state, so every voter answers
/// identically at the same applied index.
pub(crate) fn genesis_applied_fact(state: &rows::DurableState) -> bool {
    if state.model.admin_revision == 0 {
        return false;
    }
    if state.recovered_from.is_none() {
        return true;
    }
    state
        .model
        .audit
        .values()
        .any(|row| row.operation_code == crate::coordination::contract::OPERATION_GENESIS)
}

/// The one body behind [`SuiteStateMachine`], and behind every storage test
/// that installs a snapshot without a network in front of it. It adds no path
/// production lacks: the same `prepare_install`, then the same
/// `install_snapshot` with the resulting `Prepared`.
///
/// Preparation's two failures are flattened into one `RaftStorageError` here
/// because that is what the trait returns — the split still exists on the
/// production path, where the caller is the snapshot receiver and *does* answer
/// them differently. A test asserting the store kept serving still asserts
/// something real: a rejection does not latch the store fatal, and a storage
/// failure does.
#[cfg(test)]
impl CoordinationStateMachine {
    /// The adapter body, as an inherent method so the storage tests can reach
    /// it on the machine they already hold.
    ///
    /// The trailing underscore keeps it visibly not-a-trait-method: this is a
    /// test seam, and a name that read like `install_snapshot` would invite the
    /// belief that production installs this way.
    pub(crate) async fn prepare_then_install_(
        &mut self,
        meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>,
        source: Box<CoordinationSnapshotFile>,
    ) -> Result<(), RaftStorageError<CoordinationNodeId>> {
        let supplied = crate::coordination::raft::SnapshotMetaV1::from(meta);
        let prepared = self
            .handle
            .prepare_install(source, supplied)
            .await
            .map_err(|err| {
                // Flattened into one `StorageError` because the trait returns
                // one error type — but flattened **the way `rejected` already
                // did**, so a refusal of the sender's bytes still arrives as
                // `SnapshotRejected` and this store's own failure still arrives
                // as itself. Collapsing both into a generic I/O error here
                // would make every storage test that distinguishes them pass
                // for the wrong reason.
                io_state(match err {
                    snapshot::PrepareInstallError::Rejected(e) => StorageError::SnapshotRejected {
                        detail: e.to_string(),
                    },
                    snapshot::PrepareInstallError::Storage(e) => e,
                })
            })?;
        self.install_snapshot(
            meta,
            Box::new(CoordinationSnapshotFile::Prepared(Box::new(prepared))),
        )
        .await
    }
}

/// The seam that keeps `testing::Suite::transfer_snapshot` green **without a
/// bypass**.
///
/// The suite hands `install_snapshot` a `Published` handle directly, which the
/// production invariant refuses. The answer is this adapter around the
/// production state machine, and explicitly **not** a `cfg(test)` arm inside
/// `install_snapshot`.
///
/// The distinction is not stylistic. An arm inside the production method would
/// delete the invariant from the very type the real-Raft boundary test
/// exercises, so that test would be proving a property the shipped build does
/// not have. This runs the same production preparation in front of the same
/// production install, adds no path production lacks, and the boundary test
/// runs against `CoordinationStateMachine` itself — never against this.
#[cfg(test)]
pub(crate) struct SuiteStateMachine(pub(crate) CoordinationStateMachine);

#[cfg(test)]
impl RaftStateMachine<CoordinationTypeConfig> for SuiteStateMachine {
    type SnapshotBuilder = CoordinationSnapshotBuilder;

    async fn applied_state(&mut self) -> Result<Applied, RaftStorageError<CoordinationNodeId>> {
        self.0.applied_state().await
    }

    async fn apply<I>(
        &mut self,
        entries: I,
    ) -> Result<Vec<CommandReplyV1>, RaftStorageError<CoordinationNodeId>>
    where
        I: IntoIterator<Item = Entry> + Send,
        I::IntoIter: Send,
    {
        self.0.apply(entries).await
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.0.get_snapshot_builder().await
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<CoordinationSnapshotFile>, RaftStorageError<CoordinationNodeId>> {
        self.0.begin_receiving_snapshot().await
    }

    /// The raw role in, production preparation, production install.
    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>,
        snapshot: Box<CoordinationSnapshotFile>,
    ) -> Result<(), RaftStorageError<CoordinationNodeId>> {
        self.0.prepare_then_install_(meta, snapshot).await
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<CoordinationTypeConfig>>, RaftStorageError<CoordinationNodeId>>
    {
        self.0.get_current_snapshot().await
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

    const ACTOR: [u8; 32] = [0x0A; 32];
    const CLIENT: [u8; 16] = [0x0C; 16];
    const LEADER: [u8; 32] = [0x11; 32];

    use super::super::fixture::SuiteFixture;
    use super::super::SuiteBuilder;

    use super::super::scratch::ScratchDir;

    fn scratch_dir() -> ScratchDir {
        ScratchDir::new("machine")
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

    /// Every key a state holds, in every section.
    ///
    /// A change set that named only three of the seven wrote three of the seven
    /// and left the counters describing a state the rows do not add up to --
    /// which `validate` then reports, confusingly, as corruption.
    fn every_key_in(state: &DurableState) -> ChangeSet {
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
        changes
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

        rows::commit(&database, state.clone(), &every_key_in(&state)).expect("seed");
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
        super::blank_entry(LEADER, index)
    }

    fn uniform_membership(voters: &[[u8; 32]]) -> Membership<CoordinationNodeId, CoordinationNode> {
        let ids: std::collections::BTreeSet<CoordinationNodeId> = voters
            .iter()
            .map(|v| CoordinationNodeId::from_array(*v))
            .collect();
        let nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
            ids.iter().map(|id| (*id, CoordinationNode)).collect();
        Membership::new(vec![ids], nodes)
    }

    fn membership_entry(index: u64, voters: &[[u8; 32]]) -> Entry {
        Entry {
            log_id: log_id(index),
            payload: EntryPayload::Membership(uniform_membership(voters)),
        }
    }

    /// The entry `initialize` writes -- **exactly**, including its log id.
    ///
    /// `Raft::initialize` appends `Entry::new_membership(LogId::default(), ..)`
    /// (`raft_core.rs:649`), so index 0 carries term 0 and the default node id,
    /// not the term and leader a test happens to be using elsewhere. A route
    /// that means to be evidence about production recovery has to start from
    /// the log production writes, and this is the only entry in it a voter does
    /// not choose.
    fn initialize_entry(voters: &[[u8; 32]]) -> Entry {
        use openraft::entry::RaftEntry as _;
        Entry::new_membership(RaftLogId::default(), uniform_membership(voters))
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

    /// **A committed genesis this group cannot accept records nothing.**
    ///
    /// The deterministic half of the integration gate
    /// `a_different_genesis_is_fatal_and_writes_nothing`: end to end, the
    /// applied index is not readable evidence for this rule, because a blank
    /// entry from a re-election legally advances it at any moment (§11.3). Here
    /// the machine receives exactly one entry and no elections exist, so
    /// `last_applied` staying unmoved is exactly the claim -- a voter that
    /// refused a committed genesis must not record having applied it.
    #[tokio::test]
    async fn a_foreign_genesis_is_fatal_and_records_nothing() {
        use ed25519_dalek::Signer as _;

        let dir = scratch_dir();
        let (mut machine, shutdown, _drain) = machine(&dir);

        // A structurally valid genesis signed by a ROOT this store is not
        // bound to, so it fails the first of apply's checks: the signature
        // under the replicated key.
        let genesis = crate::coordination::model::authority::testing::genesis_for(
            &[[1u8; 32], [2u8; 32], [3u8; 32]],
            crate::coordination::model::shape::PROFILE_PROD,
        );
        let document_bytes =
            crate::coordination::model::authority::canonical_document_bytes(&genesis)
                .expect("encode a genesis");
        let foreign_root = ed25519_dalek::SigningKey::from_bytes(&[0x66u8; 32]);
        let signature = foreign_root
            .sign(&crate::coordination::model::authority::genesis_signing_bytes(&document_bytes));
        let signed_genesis = crate::coordination::model::authority::signed_document_from_parts(
            document_bytes,
            &foreign_root.verifying_key().to_bytes(),
            &signature.to_bytes(),
        )
        .expect("encode a signed genesis");

        let err = machine
            .apply([normal(
                1,
                CommandV1::Genesis(crate::coordination::contract::GenesisCommandV1 {
                    signed_genesis,
                }),
            )])
            .await
            .expect_err("a foreign genesis");
        assert!(format!("{err:?}").contains("GenesisRefused"), "{err:?}");
        assert!(machine.handle().is_fatal());

        // And nothing was applied: `last_applied` must not record an entry
        // this voter refused to interpret.
        drop(shutdown);
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

    /// **A native membership entry naming ROOT is refused before it is
    /// written** (§8.1), in either role.
    ///
    /// The durable half of the rule. `verify_genesis` and `plan_shape` refuse a
    /// genesis and a plan that name ROOT, but neither is on this path: a forced
    /// entry from a non-conforming leader reaches the state machine directly.
    /// A rule enforced only at admission is a rule a store can be made to hold
    /// in violation of — and this store would then apply cleanly and fail to
    /// reopen, since the loader refuses the same shape from the same predicate.
    #[tokio::test]
    async fn a_membership_entry_naming_root_is_refused_before_it_is_written() {
        use openraft::Membership;

        for as_voter in [true, false] {
            let dir = scratch_dir();
            let (mut machine, shutdown, _drain) = machine(&dir);

            let root = CoordinationNodeId::from_array(
                crate::coordination::storage::fixture::authority_public_key(),
            );
            let mut voters: std::collections::BTreeSet<CoordinationNodeId> = (1u8..=3)
                .map(|b| CoordinationNodeId::from_array([b; 32]))
                .collect();
            if as_voter {
                voters.insert(root);
            }
            let mut nodes: std::collections::BTreeMap<CoordinationNodeId, CoordinationNode> =
                voters.iter().map(|id| (*id, CoordinationNode)).collect();
            // As a learner: in the node map, in no configuration.
            nodes.insert(root, CoordinationNode);

            let err = machine
                .apply([Entry {
                    log_id: log_id(1),
                    payload: EntryPayload::Membership(Membership::new(vec![voters], nodes)),
                }])
                .await
                .expect_err("ROOT is never a committed member");
            assert!(
                format!("{err:?}").contains("ROOT"),
                "the refusal must name the rule: {err:?}"
            );
            assert!(machine.handle().is_fatal());

            drop(shutdown);
            drop(machine);
            let (mut machine, _shutdown, _drain) = reopen(&dir);
            let (applied, membership) = machine.applied_state().await.expect("applied state");
            assert_eq!(applied, None, "nothing was written");
            assert!(membership.membership().nodes().next().is_none());
        }
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
            older.meta().last_log_id.as_ref().map(|id| id.index),
            Some(1),
            "the stale transaction still sees the version it pinned"
        );
        let discarded = older.path().to_path_buf();

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
                .meta()
                .clone(),
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

    /// A leader's snapshot becomes this follower's state, marker and all.
    ///
    /// The whole of §11.4's install, through the trait method OpenRaft calls.
    /// The source is a foreign store's `Published` handle -- which is what the
    /// suite hands install -- and the assertions are about the receiver
    /// afterwards: its state is the sender's, its marker names the sender's
    /// snapshot with the metadata OpenRaft supplied, and the body under that
    /// name is byte-identical to the sender's rather than a re-encoding of the
    /// database rebuilt from it.
    #[tokio::test]
    async fn a_snapshot_installs_over_this_store_s_state() {
        let source_dir = scratch_dir();
        let (source, published) = a_published_source(&source_dir).await;
        let sender = durable_of(&source.0).await;
        let sender_bytes =
            std::fs::read(body_path(&source_dir, &published.meta.snapshot_id)).expect("the body");

        // A receiver of the same group that has applied nothing.
        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = machine(&dir);
        assert!(durable_of(&machine).await.model.last_applied.is_none());

        machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect("the install succeeds");

        let after = durable_of(&machine).await;
        assert_eq!(after, sender, "the receiver now holds the sender's state");
        assert_seven_sections(&after, "after the install");

        let current = machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("the receiver has a current snapshot");
        assert_eq!(
            current.meta, published.meta,
            "and returns the metadata OpenRaft supplied, unchanged"
        );
        assert_eq!(
            body_of(&current.snapshot),
            sender_bytes,
            "the published body is the sender's bytes verbatim"
        );
        assert!(
            leftovers(&dir).is_empty(),
            "and the staging that produced it is gone"
        );

        // Durable: the marker and the state survive a reopen.
        drop(current);
        drop(machine);
        drop(drain);
        drop(shutdown);
        let (machine, _shutdown, _drain) = reopen(&dir);
        assert_eq!(durable_of(&machine).await, sender);
    }

    /// An install and a builder in flight together both finish.
    ///
    /// The lock cycle this exists to rule out, driven rather than argued. A
    /// builder holds its read lease across `publish`, which takes the
    /// publication mutex; an install that took that mutex *before* waiting out
    /// the leases would be holding what the builder needs while waiting for
    /// what the builder holds. Neither is doing anything wrong and neither ever
    /// finishes.
    ///
    /// So: a lease is taken first, the install is sent and blocks quiescing,
    /// and only then does the builder stream, publish and release. Under the
    /// correct order the publish goes straight through and the install
    /// proceeds; under the reverse it never returns, and the bounded wait turns
    /// that into a failure instead of a hung suite.
    #[tokio::test]
    async fn an_install_and_a_builder_in_flight_together_both_finish() {
        use std::sync::mpsc;
        use std::time::Duration;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);

        // A lease, held: the install cannot complete until this goes.
        let read = machine
            .handle()
            .read_transaction()
            .await
            .expect("a read transaction");

        // The install starts and blocks in `quiesce`, holding no mutex.
        let installing = {
            let mut machine = machine.clone();
            let meta = published.meta.clone();
            let source = published.snapshot;
            tokio::spawn(async move { machine.prepare_then_install_(&meta, source).await })
        };

        // Wait until the install is actually inside `quiesce`. Without this the
        // builder usually publishes before the install has reached either lock,
        // and the interleaving that matters never happens -- which is exactly
        // what let a reversed order pass this test the first time it was
        // written.
        for _ in 0..10_000 {
            if machine.reads.admission() == super::super::reads::Admission::Quiesced {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert_eq!(
            machine.reads.admission(),
            super::super::reads::Admission::Quiesced,
            "the install never reached its wait"
        );

        // The builder's remaining work, off the actor and on its own thread so
        // a blocked publish fails the wait rather than the runtime.
        let (done, finished) = mpsc::channel();
        let builder = {
            let layout = machine.layout.clone();
            let machine = machine.clone();
            std::thread::spawn(move || {
                let built = snapshot::build(&layout, &read).expect("stream");
                let lease = read.release_transaction();
                // The publication mutex. Blocked forever if the install took it
                // before waiting for this very lease.
                let published = machine.publish(&lease, built).expect("publish");
                done.send(published.meta.snapshot_id.clone())
                    .expect("report");
                drop(lease);
            })
        };

        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the builder publishes while the install waits for its lease");
        builder.join().expect("join");

        installing
            .await
            .expect("join")
            .expect("and the install completes once the lease is released");
    }

    /// A source that does not survive validation is refused, and leaves this
    /// store's own state and latch untouched.
    ///
    /// §11.4 puts the decoder and the bounds checks in front of install so a
    /// corrupt or hostile snapshot is *rejected*. Each case corrupts a real
    /// published body and installs it through the real path, so what refuses it
    /// is the production decoder rather than a fixture asserting its own
    /// expectations.
    ///
    /// **What this does not prove.** It calls the state machine directly, so
    /// "the store is not fatal" is a statement about Aster's actor and nothing
    /// more. Through OpenRaft the voter does *not* survive: any storage error
    /// out of `install_snapshot` becomes `Fatal` in its state-machine worker,
    /// and returning `Ok` instead would be worse -- OpenRaft would advance its
    /// applied and snapshot state for a snapshot that was never installed.
    ///
    /// Rejecting rather than latching is still the right classification here,
    /// and it is a precondition for the design that fixes the rest: validation
    /// and rebuild move ahead of `Raft::install_full_snapshot`, so only a
    /// sealed, already-proved snapshot ever enters OpenRaft and a refusal is
    /// answered before OpenRaft is involved. That is a Slice 3 contract and
    /// needs a real-Raft test; until it exists, no claim here extends past this
    /// actor.
    #[tokio::test]
    async fn a_source_that_fails_validation_is_refused_without_stopping_the_store() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let good = std::fs::read(body_path(&source_dir, &published.meta.snapshot_id))
            .expect("the published body");

        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("truncated mid-body", good[..good.len() / 2].to_vec()),
            ("truncated to a header", good[..8].to_vec()),
            ("a flipped byte in the framing", {
                let mut bytes = good.clone();
                bytes[1] ^= 0xff;
                bytes
            }),
            ("a flipped byte deep in the rows", {
                let mut bytes = good.clone();
                let at = good.len() * 3 / 4;
                bytes[at] ^= 0xff;
                bytes
            }),
            ("extra bytes appended", {
                let mut bytes = good.clone();
                bytes.extend_from_slice(b"and then some");
                bytes
            }),
        ];

        for (what, corrupt) in cases {
            let dir = scratch_dir();
            let (mut machine, _shutdown, _drain) = machine(&dir);
            let before = durable_of(&machine).await;

            let body = dir.join("corrupt.body");
            std::fs::write(&body, &corrupt).expect("write the corrupt source");
            let source = CoordinationSnapshotFile::incoming(body);

            let err = machine
                .prepare_then_install_(&published.meta, Box::new(source))
                .await
                .expect_err(&format!("{what}: a corrupt source must not install"));
            let detail = format!("{err:?}");
            assert!(
                detail.contains("rejected"),
                "{what}: a source failure is a rejection, not a durability question: {detail}"
            );

            assert!(
                !machine.handle().is_fatal(),
                "{what}: a peer's bad bytes must not stop this voter"
            );
            assert_eq!(
                durable_of(&machine).await,
                before,
                "{what}: and must not touch its state"
            );
            assert!(
                leftovers(&dir).is_empty(),
                "{what}: nor leave staging behind"
            );
            assert!(
                !dir.join("snapshots").join("CURRENT").exists(),
                "{what}: nor publish anything"
            );
        }
    }

    /// A source over the aggregate ceiling is refused before it is read.
    ///
    /// §12.3 wants the size refused before allocation, and the ceiling arrived
    /// as a generic `io::Error` -- indistinguishable from a failed write, so an
    /// oversized source latched the actor fatal after all the trouble taken to
    /// bound it. Sparse, because half a gigabyte of real bytes would make this
    /// a benchmark rather than a test; the length is what the precheck reads.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_source_over_the_ceiling_is_refused_before_it_is_read() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let oversized = dir.join("oversized.body");
        let handle = std::fs::File::create(&oversized).expect("create");
        handle
            .set_len(crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES + 1)
            .expect("a sparse file past the ceiling");
        drop(handle);

        let err = machine
            .prepare_then_install_(
                &published.meta,
                Box::new(CoordinationSnapshotFile::incoming(oversized)),
            )
            .await
            .expect_err("a body past the ceiling is not installable");
        let detail = format!("{err:?}");
        assert!(detail.contains("rejected"), "{detail}");
        assert!(detail.contains("exceeds"), "{detail}");

        assert!(
            !machine.handle().is_fatal(),
            "an oversized message is the sender's problem, not this store's"
        );
        assert!(
            leftovers(&dir).is_empty(),
            "and nothing was staged before the refusal"
        );

        // Refused *before* it is read, not merely refused. With `incoming/`
        // unwritable, staging cannot even begin -- so a run that reaches the
        // copy fails as a local I/O error and latches fatal, while one that
        // checks the declared length first still reports the ceiling. Without
        // this the streaming bound catches the same file a moment later and
        // the precheck could be deleted unnoticed.
        let sealed_dir = scratch_dir();
        let (mut sealed, _s, _d) =
            CoordinationStateMachine::new(seeded(&sealed_dir), StorageLayout::new(&sealed_dir))
                .expect("open");
        let oversized = sealed_dir.join("oversized.body");
        let handle = std::fs::File::create(&oversized).expect("create");
        handle
            .set_len(crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES + 1)
            .expect("sparse");
        drop(handle);

        let incoming = sealed_dir.join("incoming");
        std::fs::create_dir_all(&incoming).expect("incoming");
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o555)).expect("chmod");
        let err = sealed
            .prepare_then_install_(
                &published.meta,
                Box::new(CoordinationSnapshotFile::incoming(oversized)),
            )
            .await
            .expect_err("still refused");
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let detail = format!("{err:?}");
        assert!(
            detail.contains("exceeds"),
            "the size is checked before anything is written: {detail}"
        );
        assert!(
            !sealed.handle().is_fatal(),
            "so an unwritable staging directory is never reached"
        );
    }

    /// An install sizes and reads the same bytes, even when the name moves.
    ///
    /// The regression for a check that was added one function above the
    /// documentation forbidding it: the ceiling precheck asked the *path* how
    /// long the source was, while the copy read the *descriptor*. For a
    /// `Published` handle those are routinely different files -- its name may
    /// have been retired, or may name a successor, while the handle pins the
    /// original bytes -- so the bound applied to one file and the copy took
    /// another.
    ///
    /// Driven by replacing the name beneath a pinned handle with a sparse file
    /// past the ceiling. Sizing by name refuses it; sizing by descriptor
    /// installs the bytes the handle actually holds.
    #[tokio::test]
    #[cfg(unix)]
    async fn an_install_sizes_the_descriptor_it_reads_not_the_name() {
        let source_dir = scratch_dir();
        let (source, published) = a_published_source(&source_dir).await;
        let sender = durable_of(&source.0).await;
        let name = body_path(&source_dir, &published.meta.snapshot_id);

        // The handle is open; the *name* now points at a different, enormous
        // file. Renamed rather than created in place: `File::create` truncates
        // the inode the handle holds, which would change the pinned bytes
        // themselves and test nothing about names at all.
        let usurper = source_dir.join("usurper");
        let handle = std::fs::File::create(&usurper).expect("create");
        handle
            .set_len(crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES + 1)
            .expect("sparse");
        drop(handle);
        std::fs::rename(&usurper, &name).expect("take the name");
        assert!(
            std::fs::metadata(&name).expect("stat").len()
                > crate::coordination::rowstream::MAX_SNAPSHOT_BODY_BYTES,
            "the name is now oversized"
        );

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect("the pinned bytes are what is sized and what is read");

        assert_eq!(
            durable_of(&machine).await,
            sender,
            "and they are the sender's state, not whatever took the name"
        );
    }

    /// A body whose rows are valid and whose counters are not is the sender's
    /// fault too.
    ///
    /// It passes everything `receive` checks -- framing, digest, identity,
    /// supplied metadata -- and fails in the rebuild, when `rows::load` runs
    /// §12.1's whole-state checks against what arrived. Still somebody else's
    /// bytes, so still a rejection: the boundary is the exclusion, not the
    /// function that happens to notice.
    #[tokio::test]
    async fn a_source_that_fails_the_rebuild_is_refused_the_same_way() {
        let valid_dir = scratch_dir();
        let ((valid, _v, _d), _) = a_published_source(&valid_dir).await;
        let mut broken = durable_of(&valid).await;
        broken.model.growth_bytes = broken.model.growth_bytes.saturating_add(1);

        let broken_dir = scratch_dir();
        let database = seeded_with(&broken_dir, broken);
        let read = database.begin_read().expect("read");
        let body = super::super::snapshot::build(&StorageLayout::new(&broken_dir), &read)
            .expect("row-valid state still has a canonical body");
        drop(read);
        drop(database);

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        let before = durable_of(&machine).await;

        let meta = rows::snapshot_meta_from_wire(body.meta()).expect("meta");
        let source = CoordinationSnapshotFile::incoming(body.path().to_path_buf());
        let err = machine
            .prepare_then_install_(&meta, Box::new(source))
            .await
            .expect_err("counters that do not describe the rows are not installable");
        let detail = format!("{err:?}");
        assert!(detail.contains("rejected"), "{detail}");
        assert!(detail.contains("aggregate-counters"), "{detail}");

        assert!(
            !machine.handle().is_fatal(),
            "a peer's inconsistent body must not stop this voter"
        );
        assert_eq!(durable_of(&machine).await, before);
    }

    /// A local failure while staging is *not* reclassified.
    ///
    /// The other side of the same line. Reclassifying the codec's verdict is
    /// right because the bytes are the sender's; reclassifying an I/O failure
    /// would be wrong for exactly the same reason, since a disk this store
    /// cannot write to is its own problem however the work arrived.
    ///
    /// Unix-only because it needs a directory this process cannot write.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_local_failure_while_staging_still_stops_the_store() {
        use std::os::unix::fs::PermissionsExt as _;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        let incoming = dir.join("incoming");
        std::fs::create_dir_all(&incoming).expect("incoming");
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o555)).expect("chmod");

        let err = machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect_err("staging cannot write");
        std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let detail = format!("{err:?}");
        assert!(
            !detail.contains("rejected"),
            "this store's own disk failing is not the sender's fault: {detail}"
        );
        assert!(
            machine.handle().is_fatal(),
            "and it is a durability question, like every other failure to write"
        );
    }

    /// A replacement that lands the wrong database stops the install dead.
    ///
    /// M203's oracle, and the boundary no external fixture can reach: the
    /// candidate is proved *before* the rename and the file is the store's
    /// after it, so the interval between belongs to one function. A crash
    /// there, or a filesystem that put something else in place, leaves a
    /// database that opens, passes integrity, carries a correct binding and
    /// loads a well-formed state -- every check before the comparison passes on
    /// it.
    ///
    /// Driven by substituting a *real database* rather than forcing an error,
    /// so what runs afterwards is the production reopen, the production load
    /// and the production comparison. The assertions are about what the install
    /// then does: it fails, it latches fatal before replying, and it publishes
    /// nothing.
    #[tokio::test]
    async fn a_replacement_that_lands_the_wrong_database_fails_the_install() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        // A valid database of the same group, holding different state: what a
        // botched rename might leave behind.
        let other_dir = scratch_dir();
        drop(seeded_with(&other_dir, seven_section_state()));

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        let before = durable_of(&machine).await;
        assert!(
            !dir.join("snapshots").join("CURRENT").exists(),
            "nothing published yet"
        );

        let _swap = super::super::snapshot::fault::swap_installed_state(
            dir.join("state.redb"),
            other_dir.join("state.redb"),
        );
        let err = machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect_err("a database that is not the installed one is not installed");
        assert!(
            format!("{err:?}").contains("not the one this install validated"),
            "{err:?}"
        );

        assert!(
            machine.handle().is_fatal(),
            "a replacement whose result cannot be identified stops the store"
        );
        assert!(
            !dir.join("snapshots").join("CURRENT").exists(),
            "and publishes nothing: a marker here would name a body the state does not match"
        );
        assert!(machine.applied_state().await.is_err(), "nor serves further");
        let _ = before;
    }

    /// Applying a batch and applying it one entry at a time across restarts
    /// reach the same state.
    ///
    /// A batch-partition property: the split into `apply` calls, and the store
    /// lifetimes between them, must not be visible in what is committed. It was
    /// called an equivalence test for log replay and is not one -- nothing here
    /// appends to `raft.redb` or recovers through OpenRaft. The real replay
    /// route is `every_route_to_a_state_agrees`; this is the cheaper property
    /// it rests on, and worth keeping separately because it isolates the
    /// batching from the recovery.
    #[tokio::test]
    async fn a_batch_and_its_partition_across_restarts_agree() {
        let entries = || {
            vec![
                normal(2, acquire(1)),
                blank(3),
                blank(4),
                normal(5, acquire(2)),
            ]
        };

        let batched_dir = scratch_dir();
        let (batched, batched_replies) = {
            let (mut machine, shutdown, drain) = CoordinationStateMachine::new(
                seeded_with(&batched_dir, seven_section_state()),
                StorageLayout::new(&batched_dir),
            )
            .expect("open");
            let replies = machine.apply(entries()).await.expect("apply");
            let state = durable_of(&machine).await;
            drop(machine);
            drop(drain);
            drop(shutdown);
            (state, replies)
        };

        let split_dir = scratch_dir();
        let mut split_replies = Vec::new();
        {
            let mut remaining = entries();
            let mut store = Some(
                CoordinationStateMachine::new(
                    seeded_with(&split_dir, seven_section_state()),
                    StorageLayout::new(&split_dir),
                )
                .expect("open"),
            );
            while !remaining.is_empty() {
                let (mut machine, shutdown, drain) = store.take().expect("a store");
                split_replies.extend(
                    machine
                        .apply([remaining.remove(0)])
                        .await
                        .expect("apply one"),
                );
                drop(machine);
                drop(drain);
                drop(shutdown);
                store = Some(reopen(&split_dir));
            }
            let (machine, shutdown, drain) = store.take().expect("a store");
            drop(machine);
            drop(drain);
            drop(shutdown);
        }

        let split = {
            let (machine, _s, _d) = reopen(&split_dir);
            durable_of(&machine).await
        };
        assert_eq!(split, batched, "the partition is not visible in the state");
        assert_eq!(
            split_replies, batched_replies,
            "nor in the replies: a client cannot tell how its entry was batched"
        );
        assert_eq!(
            canonical_digest_of(&split_dir),
            canonical_digest_of(&batched_dir),
            "nor in what the state hashes to"
        );
    }

    /// Every route to a state agrees: direct apply, committed-log replay
    /// through OpenRaft's own recovery, and snapshot install.
    ///
    /// §16's equivalence requirement. The replay route is the one that has to
    /// be real: entries are **appended to `raft.redb`** and the committed log
    /// id persisted, the state is left behind, the store is closed, and the
    /// reopen runs `StorageHelper::get_initial_state` -- OpenRaft's own
    /// reapply path -- rather than anything this suite wrote. `VoterStorage`
    /// alone would not do it: it opens and validates the two actors and
    /// performs no replay, so a test that inspected its result would be
    /// observing the state *before* recovery.
    ///
    /// Replies are compared too, and for the replay and install routes that
    /// means the saved-reply projection: the same client command is submitted
    /// again afterwards, and the session classifier must answer with the reply
    /// the original apply produced. A route that reached the right state and
    /// lost its saved replies would satisfy every byte comparison here.
    #[tokio::test]
    async fn every_route_to_a_state_agrees() {
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;
        use openraft::StorageHelper;

        // Index 0 is the membership entry `initialize` writes. Without it the
        // log starts at 1 and recovery refuses to read `[0, n)` -- which is
        // OpenRaft telling the truth about a log no real voter could have.
        let voters: Vec<[u8; 32]> = (1u8..=3).map(|b| [b; 32]).collect();
        let entries = || {
            vec![
                initialize_entry(&voters),
                normal(1, acquire(1)),
                blank(2),
                normal(3, acquire(2)),
            ]
        };
        // Submitted again after each route, to draw the saved reply back out.
        // It has to be the **latest** sequence: a session keeps one saved reply,
        // so resubmitting an earlier sequence is classified `SequenceTooOld`
        // and never reaches it.
        let replay_of = normal(4, acquire(2));

        // ── Direct: apply, then snapshot for the third route ────────────────
        let direct = scratch_dir();
        let (direct_state, direct_replies, published) = {
            let mut storage = voter(&direct);
            let replies = storage.state_mut().apply(entries()).await.expect("apply");
            let built = {
                let mut builder = storage.state_mut().get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            let state = durable_of(storage.state()).await;
            let body =
                std::fs::read(body_path(&direct, &built.meta.snapshot_id)).expect("the built body");
            drop(storage);
            (state, replies, (built.meta, body))
        };

        // ── Replay: append and commit, apply nothing, recover on reopen ─────
        let replayed = scratch_dir();
        {
            let mut storage = voter(&replayed);
            let last = entries().last().expect("entries").log_id;
            // `blocking_append` is the supported way in -- `LogFlushed`'s
            // constructor is crate-private to OpenRaft -- and it does not
            // return until the flush callback has fired.
            storage
                .log_mut()
                .blocking_append(entries())
                .await
                .expect("append");
            storage
                .log_mut()
                .save_committed(Some(last))
                .await
                .expect("commit");
            // Deliberately not applied: the state is behind the log, which is
            // exactly the crash this route recovers from.
            assert!(
                durable_of(storage.state())
                    .await
                    .model
                    .last_applied
                    .is_none(),
                "nothing applied before the close"
            );
            drop(storage);
        }
        let replayed_state = {
            let mut storage = voter(&replayed);
            let (log, state) = storage.recovery_parts();
            StorageHelper::new(log, state)
                .get_initial_state()
                .await
                .expect("OpenRaft's own recovery reapplies the committed entries");
            let recovered = durable_of(storage.state()).await;
            drop(storage);
            recovered
        };

        // ── Install: from the body the direct route built ───────────────────
        let installed = scratch_dir();
        {
            let mut storage = voter(&installed);
            let body = installed.join("from-the-leader");
            std::fs::write(&body, &published.1).expect("the sender's bytes");
            storage
                .state_mut()
                .prepare_then_install_(
                    &published.0,
                    Box::new(CoordinationSnapshotFile::incoming(body)),
                )
                .await
                .expect("install");
            drop(storage);
        }
        let installed_state = {
            let storage = voter(&installed);
            let state = durable_of(storage.state()).await;
            drop(storage);
            state
        };

        assert_eq!(
            replayed_state, direct_state,
            "committed-log replay must reach the state direct apply did"
        );
        assert_eq!(
            installed_state, direct_state,
            "and so must installing a snapshot of it"
        );

        let direct_digest = canonical_digest_of(&direct);
        assert_eq!(
            canonical_digest_of(&replayed),
            direct_digest,
            "the replayed state must also *hash* the same: a peer compares digests"
        );
        assert_eq!(
            canonical_digest_of(&installed),
            direct_digest,
            "and so must the installed one"
        );

        // ── And the replies survive both routes ─────────────────────────────
        //
        // The same client command again. Its saved reply is what the session
        // classifier answers with, so a route that reached the right state and
        // dropped its sessions passes every comparison above and fails here.
        let saved = |dir: &std::path::Path| {
            let dir = dir.to_path_buf();
            let entry = replay_of.clone();
            async move {
                let mut storage = voter(&dir);
                let replies = storage
                    .state_mut()
                    .apply([entry])
                    .await
                    .expect("resubmit the same command");
                drop(storage);
                replies
            }
        };
        let from_direct = saved(&direct).await;
        // Against the **original**, not merely against the other two routes.
        // `acquire(2)` is the command applied at index 3, so what comes back is
        // its saved reply: the same answer with the two fields a second delivery
        // changes, and nothing else. Three routes agreeing on a uniformly wrong
        // answer would satisfy every comparison below and none above.
        assert_eq!(
            from_direct[0],
            crate::coordination::model::history::replayed_answer(&direct_replies[3]),
            "the saved reply must say what the original apply returned"
        );
        assert_eq!(
            saved(&replayed).await,
            from_direct,
            "replay must keep the saved replies, not merely the resulting rows"
        );
        assert_eq!(saved(&installed).await, from_direct, "and so must install");
    }

    /// A generated history reaches the **pure model's** state by every route.
    ///
    /// `every_route_to_a_state_agrees` proves the three routes agree with each
    /// other, which is a weaker claim than it reads as: all three end in a state
    /// the state machine computed -- install merely copies one -- so a defect
    /// shared by the wire boundary and apply would make them agree and be
    /// invisible. This test supplies the missing oracle. §16's equivalence is
    /// against the model, and the model is `model::history`, which generated
    /// these commands, encoded them without going through `command_from_wire`,
    /// and computed the expected replies and final state with `apply_command`
    /// alone.
    ///
    /// Four hand-written entries are also not a history. The generator's
    /// alphabet reaches all ten action variants, both membership actions, both
    /// system commands and `retire_client`, against three resources and three
    /// actors, over a state that already has grants -- so the commands that
    /// *succeed* are exercised, not only the ones a freshly bound voter refuses.
    ///
    /// Replies are compared **whole and typed**, not as a formatted subset. A
    /// store can reach exactly the right rows and answer wrongly, and the
    /// fields a subset drops are the ones that matter: a live register value
    /// returned on a replay is a §4.3 confidentiality break that leaves no
    /// trace in state at all.
    ///
    /// What this cannot catch, said plainly: a defect inside `apply_command`
    /// itself. §16 defines equivalence *against the model*, so the model and
    /// every route share that code by construction, and no arrangement of
    /// routes here would separate them. What it does catch is everything
    /// between the log and the model -- the wire narrowing, the projection,
    /// the row codecs, the transaction boundaries, recovery and install.
    /// Dropping `AcquireActionV1::ttl_ms` in `action_from_wire` fails this test
    /// at the state comparison and leaves `every_route_to_a_state_agrees`
    /// green.
    #[tokio::test]
    async fn a_generated_history_reaches_the_model_s_state_by_every_route() {
        use crate::coordination::model::history;
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;
        use openraft::StorageHelper;

        let history = history::sample_history(400);
        // Index 0 is not among them: the seeded state has already applied it,
        // which is what a voter that has been through `initialize` looks like.
        let entries = || {
            history
                .entries
                .iter()
                .map(|(log_id, command)| Entry {
                    log_id: raft_log_id(log_id),
                    payload: EntryPayload::Normal(command.clone()),
                })
                .collect::<Vec<_>>()
        };

        // ── Direct: apply, then snapshot for the third route ────────────────
        let direct = scratch_dir();
        let (direct_state, direct_replies, published) = {
            let mut storage = initialized(&direct, &history).await;
            // What was seeded is what a route starts from, checked rather than
            // assumed: a seed that quietly differed from the model's genesis
            // would make every downstream comparison a comparison against the
            // wrong initial condition.
            assert_durable(
                &durable_of(storage.state()).await,
                &history.genesis,
                "the seeded store",
            );
            let replies = storage.state_mut().apply(entries()).await.expect("apply");
            let built = {
                let mut builder = storage.state_mut().get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            let state = durable_of(storage.state()).await;
            let body =
                std::fs::read(body_path(&direct, &built.meta.snapshot_id)).expect("the built body");
            drop(storage);
            (state, replies, (built.meta, body))
        };

        // ── Replay: append and commit, apply nothing, recover on reopen ─────
        let replayed = scratch_dir();
        {
            let mut storage = initialized(&replayed, &history).await;
            let last = entries().last().expect("entries").log_id;
            storage
                .log_mut()
                .blocking_append(entries())
                .await
                .expect("append");
            storage
                .log_mut()
                .save_committed(Some(last))
                .await
                .expect("commit");
            drop(storage);
        }
        let replayed_state = {
            let mut storage = voter(&replayed);
            let (log, state) = storage.recovery_parts();
            StorageHelper::new(log, state)
                .get_initial_state()
                .await
                .expect("OpenRaft's own recovery reapplies the committed entries");
            let recovered = durable_of(storage.state()).await;
            drop(storage);
            recovered
        };

        // ── Install: from the body the direct route built ───────────────────
        let installed = scratch_dir();
        {
            let mut storage = initialized(&installed, &history).await;
            let body = installed.join("from-the-leader");
            std::fs::write(&body, &published.1).expect("the sender's bytes");
            storage
                .state_mut()
                .prepare_then_install_(
                    &published.0,
                    Box::new(CoordinationSnapshotFile::incoming(body)),
                )
                .await
                .expect("install");
            drop(storage);
        }
        let installed_state = {
            let storage = voter(&installed);
            let state = durable_of(storage.state()).await;
            drop(storage);
            state
        };

        // ── Against the model, not against each other ───────────────────────
        //
        // The **whole** durable state, not just `.model`. The other three fields
        // are as much a part of what a route must reach: a shared error in the
        // stored membership or in the recovery provenance would leave every
        // model comparison green and every digest comparison green, because the
        // digests were only ever compared with each other.
        assert_durable(&direct_state, &history.final_state, "direct apply");
        assert_durable(
            &replayed_state,
            &history.final_state,
            "committed-log recovery",
        );
        assert_durable(&installed_state, &history.final_state, "snapshot install");

        let direct_digest = canonical_digest_of(&direct);
        assert_eq!(canonical_digest_of(&replayed), direct_digest, "same bytes");
        assert_eq!(canonical_digest_of(&installed), direct_digest, "same bytes");

        // Reply for reply, whole and typed, against the reply the model says the
        // entry produces. Not a formatted subset: a store can reach exactly the
        // right rows and still answer wrongly, and the fields a subset drops are
        // the ones that matter -- a leaked register value is a §4.3
        // confidentiality break that no state comparison can see.
        assert_eq!(direct_replies.len(), history.replies.len());
        for (at, (actual, expected)) in direct_replies.iter().zip(&history.replies).enumerate() {
            assert_eq!(
                actual, expected,
                "the reply to entry {at} was not the model's"
            );
        }

        // And every persisted **saved reply** answers a second delivery with
        // what it saved -- one probe per session row holding a `last_result`,
        // pinned to the model's own count so a probe that silently went missing
        // is a failure rather than a shorter loop. Session rows without a saved
        // result have no answer to ask for; the state comparison above covers
        // them, tombstones included.
        //
        // This runs last and in its own pass, because resubmitting writes: a
        // route measured afterwards would be hashing a state the other two had
        // not reached.
        let probes = history::saved_sessions(&history);
        assert_eq!(
            probes.len(),
            history::saved_reply_count(&history),
            "one probe per saved reply"
        );
        assert!(!probes.is_empty(), "the history persisted no saved reply");
        let saved = |dir: std::path::PathBuf| {
            let history = &history;
            let probes = &probes;
            async move {
                let mut storage = voter(&dir);
                let replies = resubmitted(storage.state_mut(), history, probes).await;
                drop(storage);
                replies
            }
        };
        for (route, dir) in [
            ("direct apply", &direct),
            ("committed-log recovery", &replayed),
            ("snapshot install", &installed),
        ] {
            let answers = saved(dir.to_path_buf()).await;
            assert_eq!(answers.len(), probes.len());
            for (answer, probe) in answers.iter().zip(&probes) {
                assert_eq!(
                    answer, &probe.expected,
                    "after {route}, the session that saved entry {} answered wrongly",
                    probe.at
                );
            }
        }
    }

    /// Resubmit the command each persisted session saved, and collect what the
    /// session classifier answers.
    ///
    /// A store that reached the right rows and lost its sessions passes every
    /// state and digest comparison and fails this one. The resubmission runs at
    /// log indices past the history's own, since a voter never re-applies an
    /// index it has already applied.
    async fn resubmitted(
        machine: &mut CoordinationStateMachine,
        history: &crate::coordination::model::history::SampleHistory,
        probes: &[crate::coordination::model::history::SavedProbe],
    ) -> Vec<CommandReplyV1> {
        let repeats: Vec<Entry> = probes
            .iter()
            .enumerate()
            .map(|(n, probe)| Entry {
                log_id: log_id(history.entries.len() as u64 + 1 + n as u64),
                payload: EntryPayload::Normal(history.entries[probe.at].1.clone()),
            })
            .collect();
        machine.apply(repeats).await.expect("resubmit the history")
    }

    /// A model log id in the form the log carries it.
    fn raft_log_id(id: &crate::coordination::model::state::LogId) -> RaftLogId<CoordinationNodeId> {
        RaftLogId {
            leader_id: CommittedLeaderId::new(
                id.term,
                CoordinationNodeId::from_array(id.leader_node_id),
            ),
            index: id.index,
        }
    }

    /// The durable state a store is **seeded** with.
    ///
    /// Its sole construction site, and deliberately not the source of the
    /// expectation. An earlier revision used one function for both, so
    /// injecting recovery provenance moved the seed and the expectation
    /// together and survived -- which is not the unavoidable limit a defect
    /// inside `apply_command` is, just a helper doing two jobs. `assert_durable`
    /// now states the expectation independently, and that mutation fails.
    fn seed_durable(model: crate::coordination::model::state::State) -> DurableState {
        DurableState {
            model,
            membership: StoredMembership::new(
                Some(RaftLogId::default()),
                uniform_membership(&crate::coordination::model::history::INITIAL_VOTERS),
            ),
            recovery_export_hash: None,
            recovered_from: None,
        }
    }

    /// Assert an observed durable state is the one a route must reach.
    ///
    /// Destructured, so a field added to `DurableState` stops compiling here
    /// rather than going silently unchecked. The membership is checked by its
    /// **parts** -- the log id index zero carries, the voter set, and the
    /// absence of learners -- rather than by rebuilding the value the seed
    /// used, and the two provenance fields are checked against literal `None`
    /// written here. Nothing in this function reads `seed_durable`.
    ///
    /// The membership is expected unchanged because nothing in a generated
    /// history moves it: a `ChangeMembership` *action* writes an intent for a
    /// controller to act on, and the `EntryPayload::Membership` entry that would
    /// move this field never arrives. The provenance is expected absent because
    /// no route here is an operator-driven §13 recovery -- install replaces a
    /// store from a peer's snapshot, which is not that.
    fn assert_durable(
        observed: &DurableState,
        model: &crate::coordination::model::state::State,
        at: &str,
    ) {
        let DurableState {
            model: seen,
            membership,
            recovery_export_hash,
            recovered_from,
        } = observed;

        assert_eq!(seen, model, "{at}: the model state");
        assert_eq!(
            membership.log_id(),
            &Some(RaftLogId::default()),
            "{at}: the stored membership must still be the one index zero committed"
        );
        let voters: Vec<[u8; 32]> = membership
            .membership()
            .voter_ids()
            .map(|id| *id.as_bytes())
            .collect();
        assert_eq!(
            voters,
            crate::coordination::model::history::INITIAL_VOTERS.to_vec(),
            "{at}: the voter set"
        );
        assert_eq!(
            membership.membership().learner_ids().count(),
            0,
            "{at}: no history here adds a learner"
        );
        assert_eq!(
            recovery_export_hash, &None,
            "{at}: no route here is a recovery, so nothing may record one"
        );
        assert_eq!(recovered_from, &None, "{at}: likewise");
    }

    /// A voter store holding the history's genesis: the state `initialize`
    /// leaves behind, and nothing more.
    ///
    /// No resources, no grants and no sessions are seeded. Those arrive as the
    /// history's own setup commands, so every route *applies* them rather than
    /// being handed their effects.
    ///
    /// Three things make the seeding legal rather than merely convenient. The
    /// first open creates and binds both databases -- a `state.redb` carrying
    /// durable state beside an absent `raft.redb` is a partial initialization
    /// and open refuses it. The seeded `StoredMembership` carries the same log
    /// id the model's `membership_log_id` does, because §11.3's validator
    /// refuses "a membership with nothing applied". And index 0 goes into the
    /// log and is **not** applied again: the seeded state has already applied
    /// it, which is what makes recovery start at index 1 the way a real reopen
    /// would.
    async fn initialized(
        dir: &std::path::Path,
        history: &crate::coordination::model::history::SampleHistory,
    ) -> super::super::VoterStorage {
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;

        drop(voter(dir));
        {
            let database = opened(dir);
            let state = seed_durable(history.genesis.clone());
            rows::commit(&database, state.clone(), &every_key_in(&state))
                .expect("seed the generated genesis");
        }
        let mut storage = voter(dir);
        let initialize = initialize_entry(&crate::coordination::model::history::INITIAL_VOTERS);
        let at = initialize.log_id;
        storage
            .log_mut()
            .blocking_append([initialize])
            .await
            .expect("append the initialization entry");
        storage
            .log_mut()
            .save_committed(Some(at))
            .await
            .expect("commit it");
        storage
    }

    // ── The process-death matrix ────────────────────────────────────────────
    //
    // See `crash.rs` for what killing a process is and is not evidence of. In
    // short: no destructor runs, which is the point; the kernel page cache
    // survives, which is why nothing here is a claim about fsync.

    use super::super::crash::{CrashPoint, Expected, Outcome};

    const SCENARIO_VAR: &str = "ASTER_COORDINATION_CRASH_SCENARIO";
    const DIR_VAR: &str = "ASTER_COORDINATION_CRASH_DIR";

    /// The durable operations the matrix crashes, the boundaries each one
    /// passes through, and what dying at each of them means.
    ///
    /// One declaration again, for the same reason the boundary list is one: the
    /// set of cases the harness runs and the set it claims to run are the same
    /// text. `every_boundary_is_covered_by_some_operation` closes the loop from
    /// the other side -- a boundary no operation reaches is a boundary the
    /// matrix would silently never exercise.
    ///
    /// The outcome lives here rather than on `CrashPoint` because it is a
    /// property of the *cell*. Two operations cross `transaction-commit` and
    /// mean opposite things by it, and one operation crosses
    /// `transaction-begin` and is repaired by replay rather than lost. A single
    /// column indexed by boundary cannot say either.
    macro_rules! operations {
        ($($(#[$doc:meta])* $variant:ident = $name:literal => [
            $($(#[$cell:meta])* $point:ident => $outcome:ident),* $(,)?
        ]),+ $(,)?) => {
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            enum Operation { $($(#[$doc])* $variant,)+ }

            impl Operation {
                const ALL: &'static [Operation] = &[$(Operation::$variant,)+];

                fn name(self) -> &'static str {
                    match self { $(Operation::$variant => $name,)+ }
                }

                fn parse(name: &str) -> Option<Self> {
                    match name { $($name => Some(Operation::$variant),)+ _ => None }
                }

                /// The boundaries this operation reaches, in order, each with
                /// what a reopen must find after dying there.
                fn cells(self) -> &'static [(CrashPoint, Outcome)] {
                    match self {
                        $(Operation::$variant => &[
                            $($(#[$cell])* (CrashPoint::$point, Outcome::$outcome),)*
                        ],)+
                    }
                }
            }
        };
    }

    operations! {
        /// Persisting a vote: one metadata transaction on `raft.redb`.
        Vote = "vote" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Durable,
        ],
        /// Appending entries to the log.
        ///
        /// One transaction, deliberately. An operation that spans two makes
        /// `TransactionCommit` ambiguous at the *operation* level -- the first
        /// commit is durable while the second never happened -- and the honest
        /// repair is a finer operation, not a wider expectation. This was found
        /// by the matrix: "append and move the marker" was one case, and killing
        /// it after the first commit left the entries durable and the marker
        /// where it started.
        Append = "append" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Durable,
        ],
        /// Moving the committed marker over entries already appended.
        ///
        /// Durable here means more than a moved marker: the entries the marker
        /// now covers are above `last_applied`, so the reopen replays them and
        /// comes back with them applied. That is the point of comparing the
        /// whole recovered view rather than the marker.
        CommitMarker = "commit-marker" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Durable,
        ],
        /// Dropping an uncommitted suffix a leader conflicted with.
        Truncate = "truncate" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Durable,
        ],
        /// Deleting an applied prefix and advancing the purge marker.
        ///
        /// Deletion and marker are one transaction, which is what makes a
        /// single expectation honest: a boundary that left the rows gone and the
        /// marker behind would be a log reporting a boundary with nothing behind
        /// it, and it would show up here as neither `before` nor `after`.
        Purge = "purge" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Durable,
        ],
        /// Applying a committed entry to `state.redb`.
        ///
        /// The operation whose cells forced the outcome off `CrashPoint`. An
        /// entry is committed in the log *before* it is applied -- that is the
        /// only order production ever runs in -- so a crash inside the apply
        /// transaction is not a lost command. redb discards the transaction, and
        /// then `get_initial_state` replays the entry the committed marker still
        /// covers. Calling those cells `Before` would assert that a voter may
        /// come back having forgotten a committed command.
        Apply = "apply" => [
            TransactionBegin => Replayed,
            TransactionWrite => Replayed,
            TransactionCommit => Durable,
            Acknowledgement => Durable,
        ],
        /// Building a snapshot body and publishing `CURRENT`.
        Build = "build" => [
            SnapshotBodyWrite => Discarded,
            SnapshotBodyFsync => Discarded,
            CurrentPublication => Durable,
        ],
        /// Installing a peer's snapshot over this store's state.
        ///
        /// The candidate is rebuilt in a database of its own under `incoming/`,
        /// through the same `durable_write`/`durable_commit` pair every other
        /// write uses -- so an install crosses the three transaction boundaries
        /// as well, and `transaction-commit` here is the moment that *candidate*
        /// became durable while `state.redb` was still untouched. It is the one
        /// cell where the boundary that means "durable" everywhere else means
        /// the operation left no trace.
        /// §13's recovery import: founding a new group from an export.
        ///
        /// The only operation that runs *inside* the open rather than against
        /// an already-open store, which is why it has boundaries of its own —
        /// by the time it starts, `schema::open` has already bound the
        /// directory and passed the three shared transaction points doing it.
        ///
        /// Its two pre-commit cells are `BoundPristine`, not `Discarded`, and
        /// the difference is the whole reason the variant exists: the
        /// replicated state really is untouched, but the identity binding is
        /// durable, so the next open is not `Fresh` and the import guard
        /// refuses a second attempt. `Discarded` would imply a retry the code
        /// forbids. The cell asserts the refusal rather than describing it.
        Import = "import" => [
            ImportBound => BoundPristine,
            ImportTransformed => BoundPristine,
            // The shared transaction seams, which belong to the import's own
            // `rows::commit` because arming happens after the bind. These are
            // the cells that make "the whole founding state or none of it" a
            // proved property rather than a claim: `TransactionWrite` stops the
            // process with every row staged *inside* the transaction, so an
            // implementation that committed resources separately from grants,
            // administrators and meta would be caught here and at
            // `TransactionCommit` — where the store would come back holding
            // part of the export. Without them, `ImportTransformed` sits before
            // the commit and `ImportCommitted` after it, and a split import
            // passes both.
            TransactionBegin => BoundPristine,
            TransactionWrite => BoundPristine,
            TransactionCommit => Durable,
            ImportCommitted => Durable,
        ],
        Install = "install" => [
            TransactionBegin => Discarded,
            TransactionWrite => Discarded,
            TransactionCommit => Discarded,
            CandidateCommit => Discarded,
            ReplacementRename => Durable,
            ReplacementDirectorySync => Durable,
            PostReplacementReopen => Durable,
            CurrentPublication => Durable,
        ],
    }

    /// Everything a reopen can read out of a store through production paths.
    ///
    /// Deliberately the whole of it rather than the field a case happens to be
    /// about: a crash in the vote transaction must leave the *log* alone too,
    /// and an assertion narrowed to the vote would not say so. The durable state
    /// carries the session rows, so saved replies are compared here as well -- a
    /// route that kept the rows and lost the replies differs in this value. The
    /// entries are the typed values OpenRaft hands back rather than a formatting
    /// of them, which is what puts each entry's term and leader id into the
    /// comparison; the pair of `(index, Debug(payload))` this replaced silently
    /// dropped both.
    ///
    /// `log_state` is the one field carried **structurally rather than proved**.
    /// It is the complete state OpenRaft asks the log for, and the purge marker
    /// in it is the one log fact no entry can show -- but no cell in the matrix
    /// distinguishes it on its own, because every operation that moves the
    /// marker also changes the entries beside it. Blanking it leaves the matrix
    /// green (mutation M260). What does hold the marker to account is the log's
    /// own tests, `purging_past_the_end_leaves_the_marker_as_the_last_log_id`
    /// and `a_regressive_purge_does_not_move_the_marker`, and the reopen itself:
    /// a purge that advanced its marker without deleting the rows produces a
    /// store `VoterStorage::open` refuses (mutation M257).
    #[derive(Debug, PartialEq)]
    struct View {
        vote: Option<openraft::Vote<CoordinationNodeId>>,
        committed: Option<RaftLogId<CoordinationNodeId>>,
        entries: Vec<Entry>,
        log_state: openraft::storage::LogState<CoordinationTypeConfig>,
        applied: Option<RaftLogId<CoordinationNodeId>>,
        membership: StoredMembership<CoordinationNodeId, CoordinationNode>,
        state: DurableState,
    }

    /// A [`View`] plus the two facts that need the store closed to read.
    #[derive(Debug, PartialEq)]
    struct Recovered {
        view: View,
        digest: [u8; 32],
        current: Option<String>,
    }

    /// What a reopen finds, on both axes.
    ///
    /// Two, because a crash has two answers and they are not always the same
    /// one. `pre_replay` is what the store holds once it is open and before
    /// OpenRaft is allowed to replay anything; `recovered` is what the voter
    /// comes back holding once it has. An observation that only recorded the
    /// second could not tell a transaction that survived from one that was
    /// thrown away and replayed; one that only recorded the first would call a
    /// replayed command a lost one.
    ///
    /// `pre_replay` and **not** `on_disk`, which is what this was first called
    /// and is a stronger claim than the harness earns.
    /// [`super::super::VoterStorage::open`] is the production open and it
    /// repairs as it goes: redb runs its own recovery over the file, `CURRENT`
    /// is validated against the bodies actually present, and the startup sweep
    /// removes scratch files and abandoned candidates. Everything this axis
    /// reports is therefore post-open. Reading the raw bytes a dead process
    /// left would be a different harness, and nothing here does it.
    #[derive(Debug, PartialEq)]
    struct Observation {
        pre_replay: View,
        recovered: Recovered,
    }

    /// Read everything an open store can be asked, through the same accessors
    /// OpenRaft uses.
    async fn view_of(storage: &mut super::super::VoterStorage) -> View {
        use openraft::storage::RaftLogReader as _;
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftStateMachine as _;

        // Through the state machine's own accessor rather than a recovered
        // `RaftState`: this is what OpenRaft asks the store on every restart, so
        // it is the value a real voter would act on.
        let (applied, membership) = storage
            .state_mut()
            .applied_state()
            .await
            .expect("the applied state");
        let mut reader = storage.log_mut().get_log_reader().await;
        let entries = reader
            .try_get_log_entries(0..u64::MAX)
            .await
            .expect("read the log back");
        let log_state = storage
            .log_mut()
            .get_log_state()
            .await
            .expect("the log state");
        let vote = storage.log_mut().read_vote().await.expect("read the vote");
        let committed = storage
            .log_mut()
            .read_committed()
            .await
            .expect("read the committed marker");
        let state = durable_of(storage.state()).await;
        View {
            vote,
            committed,
            entries,
            log_state,
            applied,
            membership,
            state,
        }
    }

    /// Open a store the way production does, read it, then run OpenRaft's own
    /// recovery and read it again.
    ///
    /// `get_initial_state` is not optional: `VoterStorage::open` starts the
    /// actors and validates the files and performs no *log replay*, so a store
    /// killed between an entry being committed and its apply would be observed
    /// in the state it had before recovery -- which is the wrong state, and the
    /// one a naive harness would compare against. Reading on both sides of it is
    /// what lets the harness say which of the two did the work.
    ///
    /// The first read is `pre_replay`, not "on disk": the open before it has
    /// already let redb recover the file and let the startup sweep tidy
    /// `snapshots/` and `incoming/`. See [`Observation`].
    async fn observe(dir: &std::path::Path) -> Observation {
        observe_as(dir, voter_identity()).await
    }

    async fn observe_as(
        dir: &std::path::Path,
        identity: super::super::identity::StorageIdentity,
    ) -> Observation {
        use openraft::StorageHelper;

        let mut storage = voter_as(dir, identity);
        let pre_replay = view_of(&mut storage).await;
        {
            let (log, state) = storage.recovery_parts();
            StorageHelper::new(log, state)
                .get_initial_state()
                .await
                .expect("OpenRaft's own recovery must run before anything is claimed");
        }
        let view = view_of(&mut storage).await;
        // Only after the store is closed: redb refuses a second open of a file
        // it already holds, and both of these need their own handle.
        drop(storage);
        Observation {
            pre_replay,
            recovered: Recovered {
                view,
                digest: canonical_digest_of(dir),
                current: current_marker(dir),
            },
        }
    }

    /// The snapshot id `CURRENT` names, if the marker is there at all.
    fn current_marker(dir: &std::path::Path) -> Option<String> {
        let layout = StorageLayout::new(dir);
        let database = opened(dir);
        let state = rows::load(&database).expect("load");
        super::snapshot::load_current(&layout, &database, &state)
            .expect("the marker must be readable or absent, never broken")
            .map(|file| file.meta().snapshot_id.clone())
    }

    /// A store set up identically for every case: seeded, initialized, and with
    /// the generated history appended, committed and applied.
    ///
    /// No snapshot is published, so `CURRENT` starts absent and the two
    /// operations that publish one are visible in the marker rather than only in
    /// the state.
    ///
    /// The setup runs in the **parent**, so the child does nothing but the one
    /// operation being crashed. A child that also did the setup would make the
    /// first `durable_write` its own, and every transaction case would be
    /// measuring the setup instead.
    async fn crash_fixture(dir: &std::path::Path) {
        let history = crate::coordination::model::history::sample_history(FIXTURE_STEPS);
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;

        let mut storage = initialized(dir, &history).await;
        let entries: Vec<Entry> = history
            .entries
            .iter()
            .map(|(log_id, command)| Entry {
                log_id: raft_log_id(log_id),
                payload: EntryPayload::Normal(command.clone()),
            })
            .collect();
        // Appended *and* applied, so the log and the state agree the way a
        // running voter's do. A fixture that only applied would leave the log
        // holding index zero alone, and an append in the operation under test
        // would be refused for a gap before it reached any boundary.
        let last = entries.last().expect("entries").log_id;
        storage
            .log_mut()
            .blocking_append(entries.clone())
            .await
            .expect("append the history");
        storage
            .log_mut()
            .save_committed(Some(last))
            .await
            .expect("commit it");
        storage
            .state_mut()
            .apply(entries)
            .await
            .expect("the setup prefix applies");
        drop(storage);
    }

    /// How many entries `crash_fixture` leaves in the log, above index zero.
    const FIXTURE_STEPS: u64 = 8;

    /// On-disk setup an operation needs that is **not** the operation.
    ///
    /// Run by the parent on all three directories, so `before` is observed from
    /// the same state the crashed store started in. Two things follow from
    /// that, and both were found by the matrix rather than reasoned into it.
    ///
    /// The first is that an operation's own preparation cannot live inside it:
    /// moving the marker presupposes entries to move it over, and appending
    /// those is a transaction of its own, so leaving it in the operation made
    /// `before` a store without them and every marker case failed for the
    /// difference rather than for the crash.
    ///
    /// The second is what the preparation has to *be*. An apply is only ever
    /// reached for an entry the log already holds as committed; applying one
    /// that is in neither is a state no voter is ever in, and a crash tested
    /// there proves nothing about the crash production would have. Staging it
    /// the way production does is also what makes the apply cells honest, since
    /// a committed entry is exactly what recovery replays.
    async fn stage_for(dir: &std::path::Path, operation: Operation) {
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;

        // Before the open, not inside the match: opening *is* binding, and the
        // import's precondition is a directory nothing has bound. Staging it
        // the way every other operation is staged would bind it to
        // `voter_identity` and the import would then be refused for an identity
        // mismatch that the harness itself created.
        if operation == Operation::Import {
            return;
        }

        let mut storage = voter(dir);
        match operation {
            // Entries for the marker to move over, left uncommitted: moving it
            // is the operation.
            Operation::CommitMarker => storage
                .log_mut()
                .blocking_append(tail_entries())
                .await
                .expect("stage the entries the marker will cover"),
            // An uncommitted suffix, which is the only kind a leader may make a
            // follower drop.
            Operation::Truncate => storage
                .log_mut()
                .blocking_append(tail_entries())
                .await
                .expect("stage the suffix the truncate removes"),
            // Appended *and* committed, because that is the only way an entry
            // ever reaches the state machine.
            Operation::Apply => {
                let entry = apply_entry();
                let at = entry.log_id;
                storage
                    .log_mut()
                    .blocking_append([entry])
                    .await
                    .expect("append the entry the apply covers");
                storage
                    .log_mut()
                    .save_committed(Some(at))
                    .await
                    .expect("commit it, as a leader would before it was applied");
            }
            Operation::Vote
            | Operation::Append
            | Operation::Purge
            | Operation::Build
            | Operation::Install
            // The import's staging is the export file, written by the parent
            // beside the directory before any of this runs: it must not have a
            // store at all, which is what `Fresh` means.
            | Operation::Import => {}
        }
        drop(storage);
    }

    /// Run one durable operation to completion, or to the boundary the
    /// environment armed.
    ///
    /// The same function on both sides. The parent calls it to learn what
    /// finishing looks like; the child calls it and is killed part-way. A
    /// harness with two implementations would be comparing a crash of one
    /// against the completion of the other.
    async fn perform(dir: &std::path::Path, operation: Operation) {
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;

        // An install needs a peer's snapshot, and building one is itself a
        // sequence of boundaries. It happens here, before the seam is armed,
        // in a store of its own -- so what the matrix crashes is the install
        // and not the build that fed it.
        let source = if operation == Operation::Install {
            Some(peer_snapshot().await)
        } else {
            None
        };

        // The import is the one operation that *is* an open, so it cannot use
        // the shared `voter(dir)` above. The seam is armed first, but the three
        // shared transaction points cannot trip it: the import's own boundaries
        // are distinct, and they are reached only after `schema::open` has
        // finished binding.
        if operation == Operation::Import {
            // Deliberately not armed here. `open_with_recovery` binds the
            // directory before the import begins, and arming across that would
            // crash the bind while calling the result an import.
            // `import_recovery_export` arms itself once the bind is done.
            let export_hash = write_import_export(dir);
            let sealed = import_capability(dir, export_hash);
            let storage = super::super::VoterStorage::open_with_recovery(
                StorageLayout::new(dir),
                super::super::identity::OpenPolicy::production(),
                &sealed.capability,
            )
            .expect("the import completes");
            drop(storage);
            return;
        }

        let mut storage = voter(dir);
        // Everything above this line is setup, including the open's own
        // transactions. The boundary the environment names starts counting now.
        super::super::crash::arm();

        match operation {
            // Handled above, before the shared open: an import *is* an open, so
            // it cannot act on the `storage` this arm holds.
            Operation::Import => unreachable!("the import returns before the shared open"),
            Operation::Vote => {
                let vote = openraft::Vote::new(7, CoordinationNodeId::from_array([9u8; 32]));
                storage
                    .log_mut()
                    .save_vote(&vote)
                    .await
                    .expect("save a vote");
            }
            Operation::Append => {
                storage
                    .log_mut()
                    .blocking_append(tail_entries())
                    .await
                    .expect("append");
            }
            Operation::CommitMarker => {
                let last = tail_entries().last().expect("entries").log_id;
                storage
                    .log_mut()
                    .save_committed(Some(last))
                    .await
                    .expect("commit the marker");
            }
            Operation::Truncate => {
                storage
                    .log_mut()
                    .truncate(tail_entries().first().expect("a suffix").log_id)
                    .await
                    .expect("truncate");
            }
            Operation::Purge => {
                storage.log_mut().purge(purge_upto()).await.expect("purge");
            }
            Operation::Apply => {
                // The entry `stage_for` appended and committed, so the operation
                // is the apply alone.
                storage
                    .state_mut()
                    .apply([apply_entry()])
                    .await
                    .expect("apply");
            }
            Operation::Build => {
                let mut builder = storage.state_mut().get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build");
            }
            Operation::Install => {
                let (meta, body) = source.expect("an install has a source");
                let staged = dir.join("from-the-leader");
                std::fs::write(&staged, &body).expect("the sender's bytes");
                storage
                    .state_mut()
                    .prepare_then_install_(
                        &meta,
                        Box::new(CoordinationSnapshotFile::incoming(staged)),
                    )
                    .await
                    .expect("install");
            }
        }
        drop(storage);
    }

    /// Two entries just past what `crash_fixture` left in the log.
    ///
    /// Contiguous with it: §4.3 refuses a gap outright, so an append at an
    /// arbitrary index would fail before reaching any boundary at all.
    fn tail_entries() -> Vec<Entry> {
        vec![blank(next_index()), blank(next_index() + 1)]
    }

    /// The first index past what `crash_fixture` left in the log.
    fn next_index() -> u64 {
        crate::coordination::model::history::SETUP_ENTRIES + FIXTURE_STEPS + 1
    }

    /// The entry the `Apply` case commits and then applies.
    ///
    /// Drawn from the same generated history one step longer, so it is the
    /// command a voter would really see at that index rather than a synthetic
    /// one -- and so the state it produces is one the pure model agrees with.
    fn apply_entry() -> Entry {
        let history = crate::coordination::model::history::sample_history(FIXTURE_STEPS + 1);
        let (log_id, command) = history.entries.last().expect("an entry").clone();
        let entry = Entry {
            log_id: raft_log_id(&log_id),
            payload: EntryPayload::Normal(command),
        };
        assert_eq!(
            entry.log_id.index,
            next_index(),
            "the applied entry must be the one that follows the fixture"
        );
        entry
    }

    /// The log id `Purge` deletes up to: the last entry of the history's setup
    /// prefix, which is far below `last_applied` and so legal to purge.
    fn purge_upto() -> RaftLogId<CoordinationNodeId> {
        let history = crate::coordination::model::history::sample_history(FIXTURE_STEPS);
        let setup = crate::coordination::model::history::SETUP_ENTRIES as usize;
        let (log_id, _) = &history.entries[setup - 1];
        let at = raft_log_id(log_id);
        assert_eq!(
            at.index,
            crate::coordination::model::history::SETUP_ENTRIES,
            "the purge boundary must be the last setup entry"
        );
        at
    }

    /// A snapshot from a peer of the same group, ahead of what the fixture
    /// applied.
    ///
    /// Ahead, because §11.4 refuses an install at or below what this store has
    /// already applied -- a source built from the same history length would be
    /// classified stale and the install would never reach a boundary.
    async fn peer_snapshot() -> (SnapshotMeta<CoordinationNodeId, CoordinationNode>, Vec<u8>) {
        let dir = scratch_dir();
        let history = crate::coordination::model::history::sample_history(FIXTURE_STEPS * 3);
        let mut storage = initialized(&dir, &history).await;
        let entries: Vec<Entry> = history
            .entries
            .iter()
            .map(|(log_id, command)| Entry {
                log_id: raft_log_id(log_id),
                payload: EntryPayload::Normal(command.clone()),
            })
            .collect();
        storage.state_mut().apply(entries).await.expect("apply");
        let built = {
            let mut builder = storage.state_mut().get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build")
        };
        let body = std::fs::read(body_path(&dir, &built.meta.snapshot_id)).expect("the body");
        drop(storage);
        (built.meta, body)
    }

    /// The child half of the matrix.
    ///
    /// A no-op unless the environment names a boundary, so it costs the ordinary
    /// suite one skipped test. When it is armed it opens the store the parent
    /// prepared, runs the one operation, and blocks inside `reached` for the
    /// parent to kill -- it never returns, and reaching the end of it means the
    /// boundary was not on the path, which the parent is told by the absence of
    /// the ready line.
    #[tokio::test]
    async fn crash_child() {
        let Ok(scenario) = std::env::var(SCENARIO_VAR) else {
            return;
        };
        let operation = Operation::parse(&scenario).expect("the parent named an operation");
        let dir =
            std::path::PathBuf::from(std::env::var(DIR_VAR).expect("the parent named a directory"));
        perform(&dir, operation).await;
        // Only reachable when the armed boundary was never hit. Said out loud,
        // because the parent's alternative diagnosis -- a child that died for
        // its own reasons -- looks identical from the outside.
        panic!("the child ran {scenario} to completion without reaching its boundary");
    }

    /// How long a child may take to reach its boundary.
    ///
    /// Generous: a child opens a store and runs a real operation, and a loaded
    /// CI host is slower than a laptop. It exists to bound a *hang*, not to
    /// time anything.
    const CHILD_TIMEOUT_SECONDS: u64 = 120;

    /// Kill a child at `point` while it runs `operation` in `dir`, and prove
    /// that is where it died.
    ///
    /// The child announces the boundary on stdout and parks; the parent kills
    /// it there. `kill` is unconditional termination -- `SIGKILL` on Unix,
    /// `TerminateProcess` on Windows -- so no destructor runs, no database is
    /// closed and nothing Rust buffered is flushed, which is the whole reason
    /// this is a subprocess rather than a `drop`. The signal number is checked
    /// only where there is one; everywhere else the portable half of the claim
    /// is that the child did not exit successfully.
    fn kill_at(dir: &std::path::Path, operation: Operation, point: CrashPoint) {
        use std::io::BufRead as _;

        let exe = std::env::current_exe().expect("the test binary");
        let mut child = std::process::Command::new(exe)
            .args([
                "--exact",
                "coordination::storage::machine::tests::crash_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(super::super::crash::BOUNDARY_VAR, point.name())
            .env(SCENARIO_VAR, operation.name())
            .env(DIR_VAR, dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn the child");

        // The read happens on its own thread with a deadline on this one, so a
        // boundary that stops being reachable fails in seconds instead of
        // blocking the reader until something else kills the child. The first
        // version had no deadline and a missed boundary cost thirteen minutes
        // before it reported. No new dependency for it: killing the child from
        // here closes its stdout, which is what releases the reader.
        let stdout = child.stdout.take().expect("piped");
        let (found, waiting) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let marker = super::super::crash::READY;
            let announced = std::io::BufReader::new(stdout).lines().find_map(|line| {
                let line = line.ok()?;
                // Searched rather than stripped: libtest writes its own
                // `test <name> ... ` with no trailing newline, so the
                // announcement can share a line with it. Matching on a prefix
                // is what made the first run of this hang.
                let at = line.find(marker)?;
                Some(line[at + marker.len()..].trim().to_owned())
            });
            let _ = found.send(announced);
        });
        let announced = waiting
            .recv_timeout(std::time::Duration::from_secs(CHILD_TIMEOUT_SECONDS))
            .unwrap_or(None);

        // Before the assertions: a child that never announced is a child still
        // running, and leaving it alive would hold the directory open for the
        // reopen below and hang the suite instead of failing it.
        //
        // The result is checked rather than dropped. A `kill` that failed would
        // leave the child parked at its boundary, `wait` would block until
        // something else ended it, and the case would be reported by a timeout
        // somewhere unrelated instead of here.
        child.kill().expect("kill the parked child");
        let status = child.wait().expect("reap the child");
        let _ = reader.join();

        let announced = announced.unwrap_or_else(|| {
            panic!(
                "the child running {} never reached {} (exit {status:?}); \
                 a boundary that is never hit is a boundary the matrix does not test",
                operation.name(),
                point.name()
            )
        });
        assert_eq!(
            announced,
            point.name(),
            "the child stopped at a different boundary than the one armed"
        );

        // It announced, so it was parked and it was this `kill` that ended it.
        // Worth stating: a child that had already exited on its own -- panicked,
        // or run past the boundary and returned -- would have closed its files
        // through the ordinary paths, and the store observed below would be the
        // product of a clean shutdown wearing a crash's name.
        assert!(
            !status.success(),
            "a killed child cannot have exited successfully: {status:?}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            // 9 is SIGKILL on every Unix this builds for, and naming the number
            // avoids a dependency on libc for one constant.
            assert_eq!(
                status.signal(),
                Some(9),
                "the child must have died by SIGKILL, not by returning: {status:?}"
            );
        }
        // Elsewhere the platform decides what an unconditional termination is;
        // `success()` above is the portable half of the same statement, and this
        // module's header claims process death rather than a particular signal.
    }

    /// Hold one observed value against the two the harness measured.
    ///
    /// The comparison is never against a literal written here. `before` is the
    /// prepared store, `after` is the same operation run to completion in a
    /// sibling directory, and the crashed store must equal one of them exactly.
    fn must_match<T: std::fmt::Debug + PartialEq>(
        expected: Expected,
        axis: &str,
        what: &str,
        crashed: &T,
        before: &T,
        after: &T,
    ) {
        match expected {
            Expected::Before => assert_eq!(
                crashed, before,
                "{what}: {axis} must be exactly what the operation started from"
            ),
            Expected::After => assert_eq!(
                crashed, after,
                "{what}: {axis} must be exactly what the completed operation leaves"
            ),
            Expected::Either => assert!(
                crashed == before || crashed == after,
                "{what}: {axis} must be one of the two states the protocol \
                 permits, and nothing else"
            ),
        }
    }

    /// What each operation must be observable *as*, stated independently of the
    /// `before`/`after` comparison.
    ///
    /// The comparison alone cannot say that the observation records the right
    /// things: two stores that differ anywhere satisfy `assert_ne!`, so a field
    /// no cell happens to distinguish can be captured and never checked. That is
    /// not hypothetical -- blanking the recorded `LogState` left the whole matrix
    /// green (mutation M260), because every operation that moves the purge marker
    /// also changes the entries beside it, and the entries carried the difference
    /// on their own.
    ///
    /// So the facts a reader would expect the matrix to be *about* are named
    /// here, per operation, against the two stores the harness prepared.
    /// Exhaustive rather than defaulted: an operation added to the table has to
    /// say what makes it observable, even if the answer is "nothing beyond the
    /// state the whole comparison already covers".
    ///
    /// Returns **how many facts it named**, which the caller pins against a
    /// declared table. Without that the assertions here are decorative: a
    /// mutation deleting one leaves the matrix green, because nothing forces an
    /// arm to assert anything at all. Found by the S5-D campaign (M39), where
    /// removing an import fact survived.
    fn distinguishing_facts(
        operation: Operation,
        before: &Observation,
        after: &Observation,
    ) -> usize {
        // The count is *produced by* asserting, never written beside it. A
        // literal count in the arm is worth nothing: deleting an assertion
        // leaves the literal, the total still matches, and the mutation
        // survives -- which is exactly what happened to the first version of
        // this guard (S5-D, M39). Going through `Facts::check` means a deleted
        // fact takes its increment with it.
        #[derive(Default)]
        struct Facts(usize);
        impl Facts {
            #[track_caller]
            fn check(&mut self, held: bool, what: &str) {
                assert!(held, "{what}");
                self.0 += 1;
            }
        }
        let mut facts = Facts::default();
        let purge_marker = |o: &Observation| o.recovered.view.log_state.last_purged_log_id;
        match operation {
            // An import turns an empty directory into a founded group, so the
            // fact that separates its two stores is the founding state itself:
            // nothing before, the exported resources after. Named here because
            // the whole-state comparison would also pass if the import had
            // written something arbitrary — this says what it must have
            // written, and that the "before" store really is empty rather than
            // merely different.
            Operation::Import => {
                facts.check(
                    before.recovered.view.state.model.resources.is_empty()
                        && before.recovered.view.state.model.admins.is_empty(),
                    "an unimported directory must hold no founding state, or the \
                     import has nothing to prove",
                );
                facts.check(
                    !after.recovered.view.state.model.resources.is_empty(),
                    "a completed import must leave the exported resources",
                );
                facts.check(
                    after.recovered.view.state.recovered_from.is_some(),
                    "a completed import must record its §13 provenance",
                );
                facts.check(
                    before.recovered.view.state.recovered_from.is_none(),
                    "and an unimported directory must record none",
                );
            }
            Operation::Purge => {
                facts.check(
                    purge_marker(before).is_none(),
                    "the fixture must start with nothing purged, or the purge \
                     has nothing to prove",
                );
                facts.check(
                    purge_marker(after) == Some(purge_upto()),
                    "a completed purge must leave the marker it advanced, and a \
                     reopen must report it",
                );
            }
            // The other operation that moves the marker, and it does so without
            // ever calling `purge`. This assertion was first written as "install
            // must not move the purge marker" and failed, which is the whole
            // argument for naming these facts: an install leaves the state
            // machine ahead of the local log, and OpenRaft closes that hole
            // itself -- `get_initial_state` purges to `last_applied` under the
            // comment "snapshot is installed but logs are not cleaned". So the
            // marker a reopened voter reports after an install is a real,
            // install-specific observable, and one no other field carries.
            Operation::Install => {
                facts.check(
                    purge_marker(before).is_none(),
                    "the fixture must start with nothing purged",
                );
                let applied = after.recovered.view.applied;
                facts.check(
                    applied > before.recovered.view.applied,
                    "the installed snapshot must be ahead of what the fixture \
                     applied, or §11.4 would have classified it stale and no \
                     install would have happened",
                );
                facts.check(
                    purge_marker(after) == applied,
                    "a reopened voter must report the marker at `last_applied` \
                     after an install, which is where OpenRaft's own recovery \
                     purges the log to",
                );
            }
            // Every other operation is fully covered by the whole-view
            // comparison: each changes entries, a marker, the durable state or
            // `CURRENT`, and `assert_ne!` above has already established that it
            // changed something. None of them leaves the state machine ahead of
            // the log, so none moves the marker.
            Operation::Vote
            | Operation::Append
            | Operation::CommitMarker
            | Operation::Truncate
            | Operation::Apply
            | Operation::Build => {
                facts.check(
                    purge_marker(before) == purge_marker(after),
                    &format!("{} must not move the purge marker", operation.name()),
                );
            }
        }
        facts.0
    }

    /// How many facts each operation must name, declared rather than counted.
    ///
    /// The other half of `distinguishing_facts`' return value. A deleted
    /// assertion changes the count and fails here, saying which operation went
    /// quiet — which is what makes those assertions load-bearing instead of
    /// decorative.
    fn expected_facts(operation: Operation) -> usize {
        match operation {
            Operation::Import => 4,
            Operation::Purge => 2,
            Operation::Install => 3,
            Operation::Vote
            | Operation::Append
            | Operation::CommitMarker
            | Operation::Truncate
            | Operation::Apply
            | Operation::Build => 1,
        }
    }

    /// Every reachable boundary of every operation, killed and recovered.
    ///
    /// Both axes are held, separately. The `pre_replay` axis says what the
    /// opened store held before OpenRaft replayed anything; the `recovered`
    /// axis says what a voter comes back holding once `get_initial_state` has
    /// run. Checking only the second would let a transaction that survived pass
    /// as one that was replayed, and checking only the first would report a
    /// replayed command as a lost one -- the two together are what makes
    /// `Replayed` a claim rather than a hedge.
    ///
    /// Neither axis defaults to a union: `Either` is declared per cell and
    /// nothing declares it, so a cell whose outcome regresses from durable to
    /// discarded fails rather than passing under a permissive comparison.
    #[tokio::test]
    async fn every_boundary_recovers_to_a_state_the_protocol_permits() {
        for operation in Operation::ALL {
            // The import founds a group from an empty directory, so it must not
            // be handed one an initialised voter already owns. Every other
            // operation is the opposite: it acts on a store that exists.
            let bootstrapped = *operation != Operation::Import;

            // An imported store is bound to its genesis's identity, not the
            // harness's default; observing it through the wrong one would fail
            // as a mismatch the harness itself caused.
            let identity = if bootstrapped {
                voter_identity()
            } else {
                import_identity()
            };

            let before_dir = scratch_dir();
            if bootstrapped {
                crash_fixture(&before_dir).await;
            }
            stage_for(&before_dir, *operation).await;
            let before = observe_as(&before_dir, identity.clone()).await;

            let after_dir = scratch_dir();
            if bootstrapped {
                crash_fixture(&after_dir).await;
            }
            stage_for(&after_dir, *operation).await;
            perform(&after_dir, *operation).await;
            let after = observe_as(&after_dir, identity.clone()).await;
            // On one axis or the other. An apply's two recovered views are
            // *equal* -- recovery replays the entry the crash lost, which is the
            // whole point -- and it is the pre-replay axis that tells the two
            // stores apart.
            assert_ne!(
                before,
                after,
                "{} changes nothing on either axis, so no boundary in it can be \
                 distinguished from any other",
                operation.name()
            );
            assert_eq!(
                distinguishing_facts(*operation, &before, &after),
                expected_facts(*operation),
                "{} named a different number of distinguishing facts than it \
                 declares; an assertion was added or deleted",
                operation.name()
            );

            // Counted, not merely called. `a_second_import_is_refused` is the
            // whole content of `BoundPristine` — the durable identity effect the
            // before/after comparison structurally cannot see — and a call whose
            // absence nothing notices asserts nothing. The S5-D campaign found
            // exactly that (M35 survived until this counter existed).
            let mut retry_refusals = 0usize;

            for (point, outcome) in operation.cells() {
                let dir = scratch_dir();
                if bootstrapped {
                    crash_fixture(&dir).await;
                }
                stage_for(&dir, *operation).await;
                kill_at(&dir, *operation, *point);
                // Asserted *before* `observe`, which opens the directory and
                // would bind one the crash had left unbound — the oracle would
                // otherwise create the very condition it is meant to detect.
                if *outcome == Outcome::BoundPristine || *operation == Operation::Import {
                    a_second_import_is_refused(&dir, *operation, *point);
                    retry_refusals += 1;
                }
                let crashed = observe_as(&dir, identity.clone()).await;

                let what = format!(
                    "{} killed at {} ({outcome:?})",
                    operation.name(),
                    point.name()
                );
                must_match(
                    outcome.pre_replay(),
                    "what the opened store held before OpenRaft replayed",
                    &what,
                    &crashed.pre_replay,
                    &before.pre_replay,
                    &after.pre_replay,
                );
                must_match(
                    outcome.recovered(),
                    "what the reopened voter recovered",
                    &what,
                    &crashed.recovered,
                    &before.recovered,
                    &after.recovered,
                );

                // The two axes above cannot, on their own, tell `Discarded`
                // from `Replayed` for an apply: the staged store and the
                // applied one recover to the *same* state -- which is the
                // property, not a defect -- so both labels satisfy the same
                // comparison. What separates them is whether the reopen had to
                // do anything, and that is a fact about the crashed store
                // alone. Asserting it here is what makes the label a claim
                // rather than a comment, and what makes swapping one for the
                // other fail.
                let owed = (
                    crashed.pre_replay.applied,
                    crashed.recovered.view.applied,
                    &crashed.pre_replay.state,
                    &crashed.recovered.view.state,
                );
                match outcome {
                    // The replicated state is untouched, exactly as
                    // `Discarded` requires; what separates the two is the
                    // durable identity binding, and that is asserted by
                    // `a_second_import_is_refused` before the observation —
                    // opening the directory here would bind it and destroy the
                    // distinction.
                    Outcome::BoundPristine => assert_eq!(
                        (owed.0, owed.2),
                        (owed.1, owed.3),
                        "{what}: nothing replicated was written, so the reopen \
                         owed the state machine nothing"
                    ),
                    Outcome::Discarded => assert_eq!(
                        (owed.0, owed.2),
                        (owed.1, owed.3),
                        "{what}: discarded means the reopen found nothing owed \
                         to the state machine and left it exactly as the crash \
                         did"
                    ),
                    Outcome::Replayed => assert_ne!(
                        owed.0, owed.1,
                        "{what}: replayed means the reopen redid the write the \
                         crash threw away; a store that came back at the same \
                         applied index never lost it in the first place"
                    ),
                    // `Durable` says nothing either way -- a committed marker
                    // is durable *and* leaves the entries it now covers for the
                    // reopen to apply -- and nothing declares `Undetermined`.
                    Outcome::Durable | Outcome::Undetermined => {}
                }
            }

            assert_eq!(
                retry_refusals,
                if *operation == Operation::Import {
                    operation.cells().len()
                } else {
                    0
                },
                "{}: every import cell must have proved that a second import is \
                 refused, and no other operation may claim to have",
                operation.name()
            );
        }
    }

    /// Every boundary is reached by some operation.
    ///
    /// The other half of the declaration: `operations!` says which boundaries
    /// each operation passes, and this says the union is the whole list. A
    /// boundary added to `crash_points!` and forgotten here would otherwise sit
    /// in the enum, be counted by the inventory test, and never be crashed.
    #[test]
    fn every_boundary_is_covered_by_some_operation() {
        let covered: std::collections::BTreeSet<&str> = Operation::ALL
            .iter()
            .flat_map(|o| o.cells())
            .map(|(point, _)| point.name())
            .collect();
        let all: std::collections::BTreeSet<&str> =
            CrashPoint::ALL.iter().map(|p| p.name()).collect();
        assert_eq!(covered, all, "a boundary no operation reaches is untested");
    }

    /// No cell is declared twice within one operation.
    ///
    /// A duplicate would arm the same boundary twice and quietly give the second
    /// declaration no effect, so the table would say more than the harness ran.
    #[test]
    fn no_operation_declares_a_boundary_twice() {
        for operation in Operation::ALL {
            let mut seen = std::collections::BTreeSet::new();
            for (point, _) in operation.cells() {
                assert!(
                    seen.insert(point.name()),
                    "{} declares {} twice",
                    operation.name(),
                    point.name()
                );
            }
        }
    }

    /// The whole matrix, spelled out.
    ///
    /// Neither of the two tests above can hold the matrix to a *size*.
    /// `every_boundary_is_covered_by_some_operation` compares unions, so
    /// deleting a whole operation, or any cell whose boundary another operation
    /// also reaches, leaves it green -- and every boundary but `acknowledgement`
    /// and `candidate-commit` is reached by more than one. Nor can either notice
    /// an outcome changing.
    ///
    /// This is the inventory: thirty-six cells, each with the operation, the
    /// boundary and what dying there means, written out rather than derived.
    /// Deleting an operation, dropping a cell, adding one, or weakening an
    /// outcome all fail here, and fail saying which. The published claim is
    /// "thirty-six kills", so the number is checked as well as the contents --
    /// but the contents first, because a count alone passes when a cell is
    /// renamed into something nobody asked for.
    ///
    /// The six `import` cells joined at S5-D — three of its own plus the three
    /// shared transaction seams, which belong to its single `rows::commit`
    /// because arming happens after the bind. The shared three are what make
    /// atomicity a *proved* property: `transaction-write` stops the process with
    /// every row staged inside the transaction, so a split import is caught
    /// rather than passing between two import-level boundaries.
    ///
    /// They are also the only cells whose outcome is not fully expressed by the
    /// before/after comparison: `BoundPristine` carries an extra obligation,
    /// asserted separately, that a second import is refused.
    #[test]
    fn the_matrix_is_the_thirty_six_cells_that_were_agreed() {
        let actual: Vec<String> = Operation::ALL
            .iter()
            .flat_map(|operation| {
                operation.cells().iter().map(move |(point, outcome)| {
                    format!("{} {} {outcome:?}", operation.name(), point.name())
                })
            })
            .collect();

        let expected = vec![
            "vote transaction-begin Discarded",
            "vote transaction-write Discarded",
            "vote transaction-commit Durable",
            "append transaction-begin Discarded",
            "append transaction-write Discarded",
            "append transaction-commit Durable",
            "commit-marker transaction-begin Discarded",
            "commit-marker transaction-write Discarded",
            "commit-marker transaction-commit Durable",
            "truncate transaction-begin Discarded",
            "truncate transaction-write Discarded",
            "truncate transaction-commit Durable",
            "purge transaction-begin Discarded",
            "purge transaction-write Discarded",
            "purge transaction-commit Durable",
            "apply transaction-begin Replayed",
            "apply transaction-write Replayed",
            "apply transaction-commit Durable",
            "apply acknowledgement Durable",
            "build snapshot-body-write Discarded",
            "build snapshot-body-fsync Discarded",
            "build current-publication Durable",
            "import import-bound BoundPristine",
            "import import-transformed BoundPristine",
            "import transaction-begin BoundPristine",
            "import transaction-write BoundPristine",
            "import transaction-commit Durable",
            "import import-committed Durable",
            "install transaction-begin Discarded",
            "install transaction-write Discarded",
            "install transaction-commit Discarded",
            "install candidate-commit Discarded",
            "install replacement-rename Durable",
            "install replacement-directory-sync Durable",
            "install post-replacement-reopen Durable",
            "install current-publication Durable",
        ];

        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 36, "the matrix is thirty-six cells");
    }

    /// Nothing is declared `Undetermined`.
    ///
    /// The variant exists so that a boundary the protocol genuinely leaves open
    /// can be *stated* as open rather than hidden inside a widened comparison.
    /// While nothing needs it, its presence is a hole: `Undetermined` weakens
    /// both axes to `Either` at once, so a cell quietly changed to it would keep
    /// passing whichever way its behaviour went.
    ///
    /// Forbidden rather than deleted. Deleting it would mean the next genuinely
    /// undetermined boundary gets a widened assertion with no name; this way
    /// using it is a deliberate act that has to come here and say what makes
    /// that cell open.
    #[test]
    fn no_cell_settles_for_an_undetermined_outcome() {
        for operation in Operation::ALL {
            for (point, outcome) in operation.cells() {
                assert_ne!(
                    *outcome,
                    Outcome::Undetermined,
                    "{} at {} is declared undetermined; if the protocol really \
                     leaves it open, say here what makes it open",
                    operation.name(),
                    point.name()
                );
            }
        }
    }

    // ── OpenRaft's own storage suite ────────────────────────────────────────

    /// The whole of `Suite::test_all` against a freshly built store.
    ///
    /// This is the conformance bar every OpenRaft storage implementation is
    /// measured against, and it is not a substitute for the tests in this
    /// crate: it checks the protocol contract -- vote monotonicity, log ranges,
    /// truncate and purge, membership recovery, snapshot transfer -- and knows
    /// nothing about §11's identity binding, row ceilings or crash boundaries.
    #[test]
    fn the_openraft_storage_suite_passes_on_a_fresh_store() {
        let root = scratch_dir();
        openraft::testing::Suite::test_all(SuiteBuilder::fresh(root.to_path_buf(), 11))
            .expect("OpenRaft's storage suite");
    }

    /// The same suite, with every store closed and reopened before use.
    ///
    /// Each store is built, dropped -- both actors joined, both databases
    /// released, the directory lock gone -- and opened again from its files
    /// before the suite is given it. What that covers is the *second open*: the
    /// whole contract holding against a store this build recovered rather than
    /// created.
    ///
    /// It is not evidence that the suite's own writes survive a restart, and
    /// should not be read as any: the reopen happens before the suite writes
    /// anything, so the store closed here is an empty one. That claim belongs to
    /// `a_normal_command_and_a_non_empty_log_survive_a_restart` below and to the
    /// crash matrix above, which reopen stores with state in them.
    #[test]
    fn the_openraft_storage_suite_passes_on_a_reopened_store() {
        let root = scratch_dir();
        openraft::testing::Suite::test_all(SuiteBuilder::reopened(root.to_path_buf(), 12))
            .expect("OpenRaft's storage suite, after a restart");
    }

    /// A `Normal` command survives a restart, and the store comes back
    /// non-empty.
    ///
    /// Separate from the suite because the suite's entries are `Blank` and
    /// `Membership`: `test_all` never appends an application command, so a
    /// store that lost every `Normal` payload on reload would pass all of it.
    /// This appends one, commits it, closes the store, reopens it, and requires
    /// the payload back -- byte for byte, through the log reader OpenRaft uses.
    #[tokio::test]
    async fn a_normal_command_and_a_non_empty_log_survive_a_restart() {
        use openraft::storage::RaftLogReader as _;
        use openraft::storage::RaftLogStorage as _;
        use openraft::storage::RaftLogStorageExt as _;

        let root = scratch_dir();
        let fixture = SuiteFixture::new(root.to_path_buf(), 13);
        let (dir, mut store) = fixture.build_at();

        let entries = vec![
            Entry {
                log_id: openraft::LogId::default(),
                payload: openraft::EntryPayload::Membership(uniform_membership(
                    &crate::coordination::model::history::INITIAL_VOTERS,
                )),
            },
            Entry {
                log_id: log_id(1),
                payload: openraft::EntryPayload::Normal(acquire(1)),
            },
        ];
        let last = entries.last().expect("entries").log_id;
        store
            .log_mut()
            .blocking_append(entries)
            .await
            .expect("append");
        store
            .log_mut()
            .save_committed(Some(last))
            .await
            .expect("commit");
        let vote = openraft::Vote::new_committed(4, *fixture.identity_for(0).local_node_id());
        store.log_mut().save_vote(&vote).await.expect("save a vote");
        drop(store);

        let mut store = fixture.open_existing(&dir);
        let mut reader = store.log_mut().get_log_reader().await;
        let back = reader
            .try_get_log_entries(0..u64::MAX)
            .await
            .expect("read the log back");
        assert_eq!(back.len(), 2, "the log came back empty or short");
        assert_eq!(back[0].log_id, openraft::LogId::default());
        match &back[1].payload {
            openraft::EntryPayload::Normal(command) => {
                assert_eq!(command, &acquire(1), "the application command changed")
            }
            other => panic!("index 1 came back as {other:?} rather than a command"),
        }
        assert_eq!(
            store.log_mut().read_committed().await.expect("committed"),
            Some(last)
        );
        assert_eq!(store.log_mut().read_vote().await.expect("vote"), Some(vote));
        drop(store);
    }

    /// The canonical digest of whatever `state.redb` a directory holds.
    fn canonical_digest_of(dir: &std::path::Path) -> [u8; 32] {
        let database = Database::create(dir.join("state.redb")).expect("reopen");
        super::super::snapshot::canonical_digest(&database).expect("hash the state")
    }

    /// A complete voter store, opened as production opens one.
    fn voter(dir: &std::path::Path) -> super::super::VoterStorage {
        voter_as(dir, voter_identity())
    }

    /// Open a store bound to a **named** identity.
    ///
    /// The matrix needs this because one operation does not use
    /// `voter_identity`: a §13 import binds the directory to the identity its
    /// own genesis produced, so observing an imported store with the harness's
    /// default identity would fail as a mismatch — a failure the harness caused
    /// rather than one the crash did.
    fn voter_as(
        dir: &std::path::Path,
        identity: super::super::identity::StorageIdentity,
    ) -> super::super::VoterStorage {
        super::super::VoterStorage::open(
            StorageLayout::new(dir),
            identity,
            super::super::identity::OpenPolicy::production(),
        )
        .expect("open voter storage")
    }

    /// **A crashed import is never retried in place** (S5-D, and the rule S5-E
    /// hands to operators).
    ///
    /// This is the obligation `Outcome::BoundPristine` carries, and it exists
    /// because the before/after comparison structurally cannot see it: `observe`
    /// opens each directory, and opening a fresh one binds it — so by the time
    /// the oracle has looked, every directory is bound and the two cases are
    /// indistinguishable. It therefore runs *before* the observation.
    ///
    /// Both sides of the matrix are covered by the same call. After a crash
    /// before the commit, the replicated state is pristine and a retry is still
    /// refused, because the identity binding is durable. After a crash once the
    /// commit returned, the founding state is complete and a retry is refused
    /// for the same reason plus a second one. Either way the answer to "just run
    /// it again" is no, and the refusal names the directory state rather than
    /// failing somewhere deeper.
    fn a_second_import_is_refused(
        dir: &std::path::Path,
        operation: Operation,
        point: super::super::crash::CrashPoint,
    ) {
        let what = format!("{} killed at {}", operation.name(), point.name());
        let sealed = import_capability(dir, canonical_import_export().1);
        // `match` rather than `expect_err`: `VoterStorage` is deliberately not
        // `Debug`, so the success arm cannot be formatted.
        let err = match super::super::VoterStorage::open_with_recovery(
            StorageLayout::new(dir),
            super::super::identity::OpenPolicy::production(),
            &sealed.capability,
        ) {
            Ok(_) => panic!(
                "{what}: a second import was accepted; recovery must be retried on a new \
                 or explicitly cleared directory, never in place"
            ),
            Err(err) => err,
        };
        let message = err.to_string();
        assert!(
            message.contains("is not fresh"),
            "{what}: the refusal must name the directory state, got: {message}"
        );
    }

    /// The node the §13 import fixture runs as. Sorted first, and one of the
    /// genesis's own initial voters — `RecoveryImport::seal` requires it.
    const IMPORT_LOCAL: [u8; 32] = [1u8; 32];

    /// The export a crashed import is importing, and the path it is read from.
    ///
    /// A sibling of the storage directory, never inside it: a stray file in a
    /// directory whose layout is under test is a variable nobody asked for.
    fn import_export_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.with_extension("export")
    }

    /// One canonical export, written where the import will look for it.
    fn write_import_export(dir: &std::path::Path) -> [u8; 32] {
        let (bytes, hash) = canonical_import_export();
        std::fs::write(import_export_path(dir), &bytes).expect("write the export");
        hash
    }

    /// The §13 capability, built the only way production can build one.
    ///
    /// Deterministically: one genesis, signed and then **verified**, the
    /// identity derived from that verified value, and `RecoveryImport::seal`
    /// asked to pair them. Constructing the capability directly would be
    /// possible from inside `import`'s own module and is deliberately not done
    /// here — the seal is the thing under test as much as the commit is, and a
    /// harness that bypassed it would be crashing a code path production
    /// cannot reach.
    /// The canonical fixture export and its hash. One definition, so the hash
    /// the genesis binds and the bytes the import reads cannot drift.
    fn canonical_import_export() -> (Vec<u8>, [u8; 32]) {
        super::super::fixture::canonical_recovery_export()
    }

    /// The identity a §13 import binds its directory to.
    ///
    /// Derived from the same verified genesis the capability is sealed from, so
    /// the harness observes an imported store through exactly the identity
    /// production wrote — not through `voter_identity`, which is another
    /// group's.
    fn import_identity() -> super::super::identity::StorageIdentity {
        import_capability(std::path::Path::new("unused"), canonical_import_export().1).identity
    }

    fn import_capability(dir: &std::path::Path, export_hash: [u8; 32]) -> ImportCapability {
        use crate::coordination::model::authority::{testing as auth, verify_genesis};

        let voters: Vec<[u8; 32]> = vec![IMPORT_LOCAL, [2u8; 32], [3u8; 32]];
        let mut genesis =
            auth::genesis_for(&voters, crate::coordination::model::shape::PROFILE_PROD);
        genesis.recovery_export_hash = Some(export_hash.to_vec());
        let signed = auth::sign_genesis(&genesis);
        let authority = auth::authority();
        let verified = verify_genesis(&authority.public_key, &authority.domain, &signed)
            .expect("the fixture genesis verifies");

        // §13 founds a *new* group, and the import refuses a body whose source
        // is the group being founded. The fixture export's source is
        // `0xAA..`; if the test authority ever adopted that id the refusal
        // would fire and the cell would be measuring the wrong thing.
        assert_ne!(
            verified.group_id(),
            &[0xAAu8; 32],
            "the fixture's new group must differ from the export's source group"
        );

        let identity = super::super::identity::StorageIdentity::new(
            crate::coordination::ids::CoordinationGroupId::from_array(*verified.group_id()),
            *verified.genesis_hash(),
            authority.public_key,
            verified.authority_domain(),
            CoordinationNodeId::from_array(IMPORT_LOCAL),
            verified.deployment_profile_code(),
        );
        let capability = super::super::import::RecoveryImport::seal(
            &identity,
            &verified,
            import_export_path(dir),
        )
        .expect("the fixture identity and genesis are one document");
        ImportCapability {
            capability,
            identity,
        }
    }

    struct ImportCapability {
        capability: super::super::import::RecoveryImport,
        identity: super::super::identity::StorageIdentity,
    }

    fn voter_identity() -> super::super::identity::StorageIdentity {
        let authority = crate::coordination::model::authority::testing::authority();
        super::super::identity::StorageIdentity::new(
            crate::coordination::ids::CoordinationGroupId::from_array(authority.group_id),
            authority.genesis_hash,
            authority.public_key,
            authority.domain.clone(),
            CoordinationNodeId::from_array([9u8; 32]),
            crate::coordination::model::shape::PROFILE_PROD,
        )
    }

    /// Two stores at the same state produce the same bytes.
    ///
    /// §11.4's determinism invariant, which every equal-index comparison rests
    /// on: if two voters at one `last_applied` could produce different bodies,
    /// the idempotent-repair path would be indistinguishable from divergent
    /// history. Independent directories, independent actors, no shared state
    /// but the entries applied.
    #[tokio::test]
    async fn two_stores_at_the_same_state_build_identical_bodies() {
        let build = |dir: std::path::PathBuf| async move {
            let (mut machine, shutdown, drain) = CoordinationStateMachine::new(
                seeded_with(&dir, seven_section_state()),
                StorageLayout::new(&dir),
            )
            .expect("open");
            machine
                .apply([normal(2, acquire(1)), blank(3)])
                .await
                .expect("the same entries");
            let built = {
                let mut builder = machine.get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            let bytes = std::fs::read(body_path(&dir, &built.meta.snapshot_id)).expect("body");
            drop(machine);
            drop(drain);
            drop(shutdown);
            (built.meta, bytes)
        };

        // Bound rather than passed inline: the guard is what removes the
        // directory, and a temporary would drop it at the end of its statement.
        let left_dir = scratch_dir();
        let right_dir = scratch_dir();
        let (left_meta, left) = build(left_dir.to_path_buf()).await;
        let (right_meta, right) = build(right_dir.to_path_buf()).await;

        // The ids first, so a regression reports two short strings rather than
        // two half-kilobyte byte vectors -- and then the bytes, because equal
        // ids with unequal bodies would be the worse failure of the two.
        assert_eq!(
            left_meta.snapshot_id, right_meta.snapshot_id,
            "the content-derived ids must agree"
        );
        assert!(
            left == right,
            "two stores at one state produced {} and {} bytes that differ; the canonical \
             body is supposed to be a pure function of replicated state",
            left.len(),
            right.len()
        );
    }

    /// The same snapshot arriving twice repairs the marker and nothing else.
    ///
    /// The lost-acknowledgement case §11.4 exists for: the install landed, the
    /// leader never heard so, and it sends the same snapshot again. The state
    /// is already correct, so replacing would rewrite a database with itself --
    /// through a quiesce, a rename and a reopen -- and every one of those is a
    /// chance to fail where nothing needed to happen.
    ///
    /// Driven by installing twice from the same sender. The second install is
    /// the one under test, and what it must *not* do is as much the point as
    /// what it must: the state database is compared byte for byte across it,
    /// because a rewrite would produce a different file holding the same state
    /// and every other assertion here would still pass.
    #[tokio::test]
    async fn the_same_snapshot_arriving_again_repairs_the_marker_only() {
        let source_dir = scratch_dir();
        let (source, first) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);
        machine
            .prepare_then_install_(&first.meta, first.snapshot)
            .await
            .expect("the first install");

        let installed = durable_of(&machine).await;
        let state_db = std::fs::read(dir.join("state.redb")).expect("the installed database");
        let body = std::fs::read(body_path(&dir, &first.meta.snapshot_id)).expect("the body");

        // The leader resends: a fresh lease on the same published snapshot.
        let again = {
            let (mut sender, _s, _d) = source;
            sender
                .get_current_snapshot()
                .await
                .expect("read")
                .expect("the sender still has it")
        };
        assert_eq!(again.meta, first.meta, "the same snapshot, not a new one");

        machine
            .prepare_then_install_(&again.meta, again.snapshot)
            .await
            .expect("the second install");

        assert_eq!(
            durable_of(&machine).await,
            installed,
            "the state is unchanged"
        );
        assert_eq!(
            std::fs::read(dir.join("state.redb")).expect("the database"),
            state_db,
            "and the database file itself was never rewritten"
        );
        assert_eq!(
            std::fs::read(body_path(&dir, &first.meta.snapshot_id)).expect("the body"),
            body,
            "nor was the published body"
        );
        assert!(
            leftovers(&dir).is_empty(),
            "and the repair left no candidate behind"
        );

        let serving = machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("still published");
        assert_eq!(serving.meta, first.meta);
    }

    /// Startup removes what previous runs abandoned, and nothing else.
    ///
    /// Every other cleanup path here is best-effort *because* this one exists:
    /// a failed promotion removes its own scratch, a superseded body is retired
    /// by its successor, a candidate is removed by its own `Drop` -- and none
    /// of that runs if the process dies first. Each file is bounded; how many
    /// of them accumulate across repeated crashes is not.
    ///
    /// Drives all four cases at once, because the interesting property is which
    /// files survive *together*: keeping the current body while removing its
    /// predecessors is one rule, not two.
    #[tokio::test]
    async fn startup_sweeps_what_earlier_runs_left_behind() {
        let dir = scratch_dir();
        let published = {
            let (mut machine, shutdown, drain) = machine(&dir);
            machine.apply([normal(1, acquire(1))]).await.expect("apply");
            let built = {
                let mut builder = machine.get_snapshot_builder().await;
                builder.build_snapshot().await.expect("build")
            };
            drop(machine);
            drop(drain);
            drop(shutdown);
            built.meta.snapshot_id.clone()
        };

        let snapshots = dir.join("snapshots");
        let incoming = dir.join("incoming");
        let current = body_path(&dir, &published);

        // A predecessor the marker no longer names: a real digest name, over
        // content that does not matter -- the sweep goes by the name, since a
        // body it is about to remove is not worth hashing.
        let stale = snapshots.join(format!("{}.snapshot", "ab".repeat(32)));
        std::fs::write(&stale, b"a superseded body").expect("stale body");

        // Scratch files from interrupted work, in both directories.
        let building = snapshots.join(".building-1-0.partial");
        let installing = incoming.join(".installing-1-0.partial");
        std::fs::write(&building, b"half a body").expect("scratch");
        std::fs::write(&installing, b"half a staged body").expect("scratch");

        // Things that are not this store's to delete. The middle one is the
        // case that matters: it carries the published suffix, so it reaches the
        // digest check rather than being filtered out before it -- which is
        // where a sweep that trusted the suffix alone would delete an
        // operator's file.
        let operator = snapshots.join("backup-before-upgrade.snapshot.bak");
        let named_like_a_body = snapshots.join("operator-copy.snapshot");
        let odd = incoming.join("notes.txt");
        std::fs::write(&operator, b"an operator's copy").expect("operator file");
        std::fs::write(&named_like_a_body, b"not a digest name").expect("lookalike");
        std::fs::write(&odd, b"someone's notes").expect("odd file");

        // Files that merely *resemble* a scratch name. None matches the grammar
        // `scratch_name` produces, and a sweep that went by "dot ... .partial"
        // deleted all of them.
        let resemblances = [
            snapshots.join(".operator-copy.partial"),
            snapshots.join(".installing-1-0.partial"), // right shape, wrong directory
            incoming.join(".building-1-0.partial"),    // likewise
            incoming.join(".receiving-notanumber-0.partial"),
            incoming.join(".receiving-1.partial"),
            incoming.join(".receiving-1-0-2.partial"),
        ];
        for path in &resemblances {
            std::fs::write(path, b"not this store's").expect("resemblance");
        }

        // And a directory shaped like one, which the sweep would not merely
        // delete wrongly but would fail startup trying to.
        let shaped_dir = incoming.join(".installing-2-0.partial");
        std::fs::create_dir(&shaped_dir).expect("a directory in scratch's clothing");

        let (machine, _shutdown, _drain) = reopen(&dir);

        assert!(current.exists(), "the body the marker names survives");
        assert!(!stale.exists(), "a body it does not name does not");
        assert!(!building.exists(), "nor an abandoned build scratch");
        assert!(!installing.exists(), "nor an abandoned staging scratch");
        assert!(operator.exists(), "an unrecognised name is left alone");
        assert!(
            named_like_a_body.exists(),
            "including one that carries the published suffix without a digest name"
        );
        assert!(odd.exists(), "in either directory");
        for path in &resemblances {
            assert!(
                path.exists(),
                "a name that only resembles a scratch file is not this store's to delete: {}",
                path.display()
            );
        }
        assert!(
            shaped_dir.is_dir(),
            "and a directory shaped like one is left alone rather than failing the open"
        );

        // And the store still serves the snapshot it kept.
        let mut machine = machine;
        let serving = machine
            .get_current_snapshot()
            .await
            .expect("read")
            .expect("still published");
        assert_eq!(serving.meta.snapshot_id, published);
    }

    /// With no marker, every digest-named body is an orphan.
    ///
    /// The case that decides whether the sweep is keyed on the marker or on
    /// something it inferred: a store that has never published has nothing to
    /// keep, and a sweep that guessed would have to guess here.
    #[tokio::test]
    async fn startup_without_a_marker_sweeps_every_body() {
        let dir = scratch_dir();
        {
            let (machine, shutdown, drain) = machine(&dir);
            drop(machine);
            drop(drain);
            drop(shutdown);
        }
        assert!(
            !dir.join("snapshots").join("CURRENT").exists(),
            "this store has never published"
        );

        let orphan = dir
            .join("snapshots")
            .join(format!("{}.snapshot", "cd".repeat(32)));
        std::fs::write(&orphan, b"a body from a run that never published").expect("orphan");

        let (_machine, _shutdown, _drain) = reopen(&dir);
        assert!(!orphan.exists(), "with no marker, no body is current");
    }

    /// A valid database that is not the installed one is refused.
    ///
    /// The reopen asks two questions, and this is the second. A rename that put
    /// *something else* in place leaves a file that opens, passes integrity,
    /// carries a correct binding and loads a well-formed state -- every check
    /// before this one passes on it. Only comparing against what the candidate
    /// was validated to contain says otherwise.
    ///
    /// Driven through a directly-assembled `ReplacedInstall`, because producing
    /// it for real needs the file to change between the rename and the reopen,
    /// which is Checkpoint E's fault injection.
    #[tokio::test]
    async fn a_reopen_refuses_a_database_that_is_not_the_one_installed() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        // A store whose `state.redb` is perfectly valid and holds other state.
        let dir = scratch_dir();
        let staged = {
            let (machine, shutdown, drain) = machine(&dir);
            let staged = super::super::snapshot::receive(
                &StorageLayout::new(&dir),
                &published.snapshot,
                &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
                &crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
            )
            .expect("stage");
            // The reopen opens `state.redb` itself, exactly as it would after a
            // real rename, so the store's own handle goes first.
            drop(machine);
            drop(drain);
            drop(shutdown);
            staged
        };

        // Claim to have installed the sender's state, over a database that
        // still holds the receiver's.
        let sender = seven_section_state();
        let replaced = super::super::snapshot::ReplacedInstall::for_test(
            sender,
            staged,
            dir.join("state.redb"),
        );
        let err = replaced
            .reopen()
            .expect_err("a valid database that is not the installed one is not installed");
        assert!(
            format!("{err:?}").contains("not the one this install validated"),
            "{err:?}"
        );
    }

    /// Promoting an installed body does not disturb a reader of what was there.
    ///
    /// The reason promotion is a rename and not a copy. A copy opens the
    /// destination and truncates it, so a reader holding that file sees its
    /// bytes change underneath -- and the file it holds is one this store told
    /// it was immutable. A rename leaves that inode alone and moves the name.
    ///
    /// Reachable because a digest name can be occupied by an orphan whose
    /// content is *not* what the name claims: an earlier run's partial write.
    /// Promotion replaces it, which is the repair -- and must not reach into
    /// the bytes an existing reader is already holding.
    #[tokio::test]
    #[cfg(unix)]
    async fn promoting_over_an_orphan_leaves_its_readers_alone() {
        use std::io::Read as _;

        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (mut machine, _shutdown, _drain) = machine(&dir);

        // An orphan under the name the install will publish to, holding
        // something else entirely, with a reader on it.
        let target = body_path(&dir, &published.meta.snapshot_id);
        std::fs::create_dir_all(dir.join("snapshots")).expect("snapshots dir");
        std::fs::write(&target, b"an earlier run's partial body").expect("orphan");
        let mut reader = std::fs::File::open(&target).expect("a reader on the orphan");

        machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect("the install succeeds");

        let mut held = Vec::new();
        reader.read_to_end(&mut held).expect("read what it holds");
        assert_eq!(
            held, b"an earlier run's partial body",
            "the reader's bytes changed underneath it: promotion copied rather than renamed"
        );
        assert_ne!(
            std::fs::read(&target).expect("the published body"),
            held,
            "while the name now points at the installed body"
        );
    }

    /// A snapshot this store has already applied past stops the store.
    ///
    /// It read as an ordinary race at first -- a leader sending a snapshot
    /// built before this follower caught up -- and was classified as a benign
    /// refusal. OpenRaft does not leave that reading open: it filters snapshots
    /// at or below `committed` before calling the state machine, so arriving
    /// here means the durable state and the state OpenRaft believes this node
    /// has already disagree. The nonfatal projection was unreachable anyway,
    /// since any error out of `install_snapshot` ends the state-machine worker.
    #[tokio::test]
    async fn an_install_behind_the_state_stops_the_store() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;

        let dir = scratch_dir();
        let (mut machine, shutdown, drain) = CoordinationStateMachine::new(
            seeded_with(&dir, seven_section_state()),
            StorageLayout::new(&dir),
        )
        .expect("open");
        machine
            .apply([blank(2), blank(3)])
            .await
            .expect("move past the candidate");
        let before = durable_of(&machine).await;

        let err = machine
            .prepare_then_install_(&published.meta, published.snapshot)
            .await
            .expect_err("a snapshot this store is past cannot be installed");
        assert!(
            format!("{err:?}").contains("roll the state machine back"),
            "{err:?}"
        );

        assert!(
            machine.handle().is_fatal(),
            "reaching this branch means OpenRaft and the durable state disagree"
        );
        assert!(
            leftovers(&dir).is_empty(),
            "with neither artifact left behind"
        );

        // Fatal for this store, and yet nothing of the refused install reached
        // disk: the state a reopen finds is the one that was there before.
        // Captured and then discarded, this assertion was missing entirely.
        drop(machine);
        drop(drain);
        drop(shutdown);
        let (machine, _shutdown, _drain) = reopen(&dir);
        assert_eq!(
            durable_of(&machine).await,
            before,
            "a refused install leaves the durable state exactly as it was"
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            let err = super::super::snapshot::receive(
                &layout,
                &published.snapshot,
                &super::super::snapshot::SnapshotIdentity::of(&state),
                &supplied,
            )
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
        super::super::snapshot::receive(
            &layout,
            &published.snapshot,
            &super::super::snapshot::SnapshotIdentity::of(&state),
            &truthful,
        )
        .expect("the metadata the body actually implies");
    }

    /// **The metadata half of the crossing.**
    ///
    /// Two valid same-group preparations, installed with body A's `Prepared`
    /// under body B's `SnapshotMeta`. Every individual value verifies — both
    /// bodies passed preparation against their own metadata — and only the
    /// binding catches it.
    ///
    /// The comparison is a *fatal invariant* rather than a rejection: by this
    /// point both values are ours, and a peer sent neither of them crossed.
    #[tokio::test]
    async fn two_valid_preparations_cannot_be_crossed_by_their_metadata() {
        let dir_a = scratch_dir();
        let (_a, published_a) = a_published_source(&dir_a).await;

        // A second source of the same group at a *different* applied index, so
        // its metadata is genuinely different while remaining valid.
        let dir_b = scratch_dir();
        let (mut source_b, _) = a_published_source(&dir_b).await;
        source_b
            .0
            .apply([normal(3, acquire(2))])
            .await
            .expect("a second command");
        let published_b = {
            let mut builder = source_b.0.get_snapshot_builder().await;
            builder.build_snapshot().await.expect("build")
        };
        assert_ne!(
            published_a.meta, published_b.meta,
            "the two preparations must differ, or the crossing is a no-op"
        );

        let dir = scratch_dir();
        let (mut receiver, _shutdown, _drain) = machine(&dir);

        // Body A, proved against A's own metadata — a completely valid
        // preparation.
        let prepared_a = receiver
            .handle
            .prepare_install(
                published_a.snapshot,
                crate::coordination::raft::SnapshotMetaV1::from(&published_a.meta),
            )
            .await
            .expect("body A prepares against its own metadata");

        // Now hand it to OpenRaft's boundary under B's metadata.
        let err = receiver
            .install_snapshot(
                &published_b.meta,
                Box::new(CoordinationSnapshotFile::Prepared(Box::new(prepared_a))),
            )
            .await
            .expect_err("a crossed pair must not install");
        assert!(
            format!("{err:?}").contains("two valid preparations were crossed"),
            "the failure must name the crossing rather than a generic mismatch: {err:?}"
        );

        // And the same body under its *own* metadata installs, so the refusal
        // above is about the crossing and not about the fixture.
        let (_a2, published_a2) = a_published_source(&scratch_dir()).await;
        let dir2 = scratch_dir();
        let (mut receiver2, _s2, _d2) = machine(&dir2);
        receiver2
            .prepare_then_install_(&published_a2.meta, published_a2.snapshot)
            .await
            .expect("an uncrossed pair installs");
    }

    /// **The property that forbids resolving during preparation.**
    ///
    /// No single-threaded install can fail this by accident: with a prepared
    /// snapshot in hand, an apply lands that moves `last_applied` **past** the
    /// candidate, and the install must be classified against the *post-apply*
    /// state.
    ///
    /// An implementation that resolved during preparation would have decided
    /// `Replace` — the candidate was ahead when it was prepared — and rolled
    /// the state machine back, with nothing downstream able to tell. Resolving
    /// on the actor under the same exclusion as the rename gives `Stale`.
    ///
    /// The two calls are deliberately separate here rather than going through
    /// the adapter: the adapter runs them back to back, which is exactly the
    /// window this test needs to open.
    #[tokio::test]
    async fn an_apply_between_preparation_and_install_decides_the_verdict() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let candidate_index = published
            .meta
            .last_log_id
            .as_ref()
            .map(|id| id.index)
            .expect("the fixture snapshot has an applied index");

        let dir = scratch_dir();
        let (mut receiver, _shutdown, _drain) = CoordinationStateMachine::new(
            seeded_with(&dir, seven_section_state()),
            StorageLayout::new(&dir),
        )
        .expect("open");

        // Prepared while this store is *behind* the candidate, so a verdict
        // taken now would be `Replace`.
        let before = durable_of(&receiver).await;
        let behind = before
            .model
            .last_applied
            .as_ref()
            .map(|id| id.index)
            .unwrap_or(0);
        assert!(
            behind < candidate_index,
            "the store must start behind the candidate, or the window does not exist"
        );
        let prepared = receiver
            .handle
            .prepare_install(
                published.snapshot,
                crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
            )
            .await
            .expect("preparation succeeds against a candidate we are behind");

        // The apply that lands in the window.
        let past: Vec<_> = (candidate_index + 1..=candidate_index + 2)
            .map(blank)
            .collect();
        receiver.apply(past).await.expect("move past the candidate");

        // And now the install, which must see the post-apply state.
        let err = receiver
            .install_snapshot(
                &published.meta,
                Box::new(CoordinationSnapshotFile::Prepared(Box::new(prepared))),
            )
            .await
            .expect_err("the candidate is now behind this store");
        assert!(
            format!("{err:?}").contains("roll the state machine back"),
            "the verdict must be taken against the post-apply state: {err:?}"
        );
    }

    /// **The vote half has no runtime test, and this states why.**
    ///
    /// `RaftStateMachine::install_snapshot(&mut self, meta, data)` has no vote
    /// parameter, and a vote is derived from nothing the receiver retains — so
    /// there is no value at this layer against which a substituted vote could
    /// be compared. A test that appeared to exercise the crossing here would be
    /// asserting a property no code path has.
    ///
    /// The obligation is carried structurally instead, by the source guards in
    /// `network::handoff`: one constructor for `SnapshotHandoff`, one call site
    /// for `install_full_snapshot`, and no constructor function that takes a
    /// vote, a `SnapshotMeta` and a `PreparedSnapshot` as separate arguments.
    /// This test asserts only the fact that makes that necessary.
    #[test]
    fn the_state_machine_never_receives_a_vote_to_compare() {
        let machine = include_str!("machine.rs");
        let at = machine
            .find("    async fn install_snapshot(")
            .expect("the state machine implements install_snapshot");
        let signature: String = machine[at..].lines().take(6).collect::<Vec<_>>().join("\n");
        assert!(
            signature.contains("meta:") && signature.contains("snapshot:"),
            "the signature under test moved: {signature}"
        );
        assert!(
            !signature.contains("vote"),
            "if install_snapshot ever received a vote, the vote-substitution obligation \
             would become checkable at runtime and this note would be wrong: {signature}"
        );
    }

    /// A raw role reaching production installation is a **fatal invariant**,
    /// not a fallback.
    ///
    /// Both raw roles, because they fail for the same reason and an
    /// implementation that special-cased one would look correct against a test
    /// that only tried the other.
    #[tokio::test]
    async fn a_raw_role_at_the_production_boundary_is_an_invariant_failure() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let dir = scratch_dir();
        let (mut receiver, _shutdown, _drain) = machine(&dir);

        let err = receiver
            .install_snapshot(&published.meta, published.snapshot)
            .await
            .expect_err("a Published role must not install");
        assert!(
            format!("{err:?}").contains("raw Published role"),
            "the failure must name the role it refused: {err:?}"
        );

        let incoming = Box::new(CoordinationSnapshotFile::incoming(
            StorageLayout::new(&dir).incoming().join("not-prepared"),
        ));
        let err = receiver
            .install_snapshot(&published.meta, incoming)
            .await
            .expect_err("an Incoming role must not install either");
        assert!(format!("{err:?}").contains("raw Incoming role"), "{err:?}");
    }

    /// Preparation's two failures are answered differently, which is the whole
    /// reason it has its own error type.
    ///
    /// A refusal of the sender's bytes leaves the store serving; this store's
    /// own staging failure does not. Both are driven here through the real
    /// handle, so the classification is the one a caller would see.
    #[tokio::test]
    async fn preparation_keeps_a_refusal_and_a_storage_failure_apart() {
        let source_dir = scratch_dir();
        let (_source, published) = a_published_source(&source_dir).await;
        let dir = scratch_dir();
        let (receiver, _shutdown, _drain) = machine(&dir);

        // A truncated body: the sender's bytes, refused.
        let staged = StorageLayout::new(&dir).incoming().join("truncated");
        std::fs::create_dir_all(staged.parent().expect("a directory")).expect("mkdir");
        let bytes = std::fs::read(body_path(&source_dir, &published.meta.snapshot_id))
            .expect("the sender's body");
        std::fs::write(&staged, &bytes[..bytes.len() / 2]).expect("truncate");

        let err = receiver
            .handle
            .prepare_install(
                Box::new(CoordinationSnapshotFile::incoming(&staged)),
                crate::coordination::raft::SnapshotMetaV1::from(&published.meta),
            )
            .await
            .expect_err("a truncated body is refused");
        assert!(
            matches!(
                err,
                super::super::snapshot::PrepareInstallError::Rejected(_)
            ),
            "a truncated body is the sender's problem, not ours: {err:?}"
        );
        assert!(
            !receiver.handle.is_fatal(),
            "and the voter keeps serving — the property the whole boundary exists for"
        );

        // The store still works afterwards.
        receiver
            .handle
            .read_transaction()
            .await
            .expect("the store still serves reads after refusing a peer's bytes");
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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

        let supplied = built.meta().clone();
        let source = CoordinationSnapshotFile::incoming(built.path().to_path_buf());
        let dir = scratch_dir();
        let (machine, _shutdown, _drain) = machine(&dir);
        let staged = super::super::snapshot::receive(
            &StorageLayout::new(&dir),
            &source,
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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

        let identity_for_receive = super::super::snapshot::SnapshotIdentity::of(&state);
        let staged_bad = super::super::snapshot::receive(
            &layout,
            &CoordinationSnapshotFile::incoming(bad.path()),
            &identity_for_receive,
            bad.meta(),
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
            &identity_for_receive,
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
            let (machine, _s, _d, dir) = receiver(dir.to_path_buf());
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
            let (mut machine, _s, _d, dir) = receiver(dir.to_path_buf());
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
            let (mut machine, _s, _d, dir) = receiver(dir.to_path_buf());
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
            let (machine, _s, _d, dir) = receiver(dir.to_path_buf());
            let durable = durable_of(&machine).await;
            assert_eq!(
                durable.model.last_applied.map(|id| id.index),
                Some(1),
                "the receiver is at the same index"
            );
            let source = CoordinationSnapshotFile::incoming(body.path());
            let prepared = prepare(&dir, &source, &durable, body.meta());
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
            let (mut machine, _s, _d, dir) = receiver(dir.to_path_buf());
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
        let staged = super::super::snapshot::receive(
            &layout,
            source,
            &super::super::snapshot::SnapshotIdentity::of(durable),
            meta,
        )
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine_a).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
            &super::super::snapshot::SnapshotIdentity::of(&durable_of(&machine).await),
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
                .meta()
                .clone(),
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
