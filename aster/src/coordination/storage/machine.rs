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
use std::sync::Arc;

use openraft::storage::RaftStateMachine;
use openraft::{
    EntryPayload, LogId as RaftLogId, RaftSnapshotBuilder, Snapshot, SnapshotMeta,
    StorageError as RaftStorageError, StorageIOError, StoredMembership,
};
use redb::Database;
use tokio::sync::{mpsc, oneshot};

use super::actor::ActorShutdown;
use super::error::{Db, StorageError, StorageResult};
use super::rows::{self, DurableState};
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
    /// Hand out a copy of the published state.
    ///
    /// Reads go through the actor rather than through a second redb read
    /// transaction so that "what is published" has one answer. A reader that
    /// opened its own transaction could observe the rows of a batch whose
    /// in-memory publication had not happened yet.
    Snapshot(Reply<Box<DurableState>>),
    Shutdown,
    /// Force a fatal failure through the ordinary serve path.
    #[cfg(test)]
    InjectFatal(Reply<()>),
    /// Arm the commit failure described on [`StateActor::fail_next_commit`].
    #[cfg(test)]
    FailNextCommit(Reply<()>),
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
                StateCommand::Snapshot(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::InjectFatal(reply) => fail(reply),
                #[cfg(test)]
                StateCommand::FailNextCommit(reply) => fail(reply),
            }
        }
    }

    /// The published state, or `Fatal` if a batch took it and never gave it
    /// back.
    fn published(&self) -> StorageResult<DurableState> {
        // Checkpoint D replaces this clone on the snapshot path with the
        // planned read-transaction stream; §11.4 forbids materialising a body
        // in memory, and a snapshot builder that cloned the state would do
        // exactly that. It stays here meanwhile because nothing on the apply
        // path uses it.
        self.state.clone().ok_or(StorageError::Fatal)
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
                    working.membership =
                        StoredMembership::new(Some(entry.log_id), membership.clone());
                    working.model.membership = rows::project_membership(membership);
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
    /// Load and validate the durable state, then start the actor on it.
    ///
    /// The load happens **before** the thread exists, for the same reason the
    /// log's validation does: a store published first and validated later has
    /// already told the group it is up by the time the first apply finds the
    /// corruption.
    pub(crate) fn spawn(database: Database) -> StorageResult<(Self, StateShutdown)> {
        let state = rows::load(&database)?;

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

    fn mark_fatal(&self) {
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

    /// A copy of the published state, for tests and for Checkpoint D's
    /// snapshot builder.
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
}

impl CoordinationStateMachine {
    pub(crate) fn new(database: Database) -> StorageResult<(Self, StateShutdown)> {
        let (handle, shutdown) = StateHandle::spawn(database)?;
        Ok((Self { handle }, shutdown))
    }

    pub(crate) fn handle(&self) -> &StateHandle {
        &self.handle
    }
}

/// The snapshot builder. Checkpoint D gives it a body; until then it exists so
/// the trait is satisfiable and the type config is exercised.
#[derive(Clone, Debug)]
pub(crate) struct CoordinationSnapshotBuilder;

impl RaftSnapshotBuilder<CoordinationTypeConfig> for CoordinationSnapshotBuilder {
    async fn build_snapshot(
        &mut self,
    ) -> Result<Snapshot<CoordinationTypeConfig>, RaftStorageError<CoordinationNodeId>> {
        Err(not_until_checkpoint_d("build_snapshot"))
    }
}

/// The snapshot paths land in Checkpoint D, and refusing is the honest
/// placeholder.
///
/// Not `unimplemented!()`: a panic inside a library is not a failure a runtime
/// can report, and OpenRaft's suite is not run against this store until
/// Checkpoint D supplies the real paths — so nothing reaches these except a
/// caller that has got ahead of the plan, which should get an error saying so.
fn not_until_checkpoint_d(what: &'static str) -> RaftStorageError<CoordinationNodeId> {
    io_state(StorageError::corrupt(
        Db::State,
        "snapshot",
        format!("{what} lands in Checkpoint D"),
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
        CoordinationSnapshotBuilder
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<Box<CoordinationSnapshotFile>, RaftStorageError<CoordinationNodeId>> {
        Err(not_until_checkpoint_d("begin_receiving_snapshot"))
    }

    async fn install_snapshot(
        &mut self,
        _meta: &SnapshotMeta<CoordinationNodeId, CoordinationNode>,
        _snapshot: Box<CoordinationSnapshotFile>,
    ) -> Result<(), RaftStorageError<CoordinationNodeId>> {
        Err(not_until_checkpoint_d("install_snapshot"))
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<CoordinationTypeConfig>>, RaftStorageError<CoordinationNodeId>>
    {
        Err(not_until_checkpoint_d("get_current_snapshot"))
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
        let database = Database::create(dir.join("state.redb")).expect("create");
        let write = super::super::schema::durable_write(&database, Db::State).expect("write");
        super::super::schema::create_state_tables(&write).expect("tables");
        write.commit().expect("commit");

        let state = genesis_state();
        let mut changes = ChangeSet::default();
        changes.admins.extend(state.model.admins.iter().copied());
        changes.grants.extend(state.model.acl.keys().cloned());
        changes
            .resources
            .extend(state.model.resources.keys().cloned());
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

    fn machine(dir: &std::path::Path) -> (CoordinationStateMachine, StateShutdown) {
        CoordinationStateMachine::new(seeded(dir)).expect("open the state machine")
    }

    /// Reopen the same directory, which is what proves a write was durable.
    fn reopen(dir: &std::path::Path) -> (CoordinationStateMachine, StateShutdown) {
        let database = Database::create(dir.join("state.redb")).expect("reopen");
        CoordinationStateMachine::new(database).expect("reopen the state machine")
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
        let (mut machine, shutdown) = machine(&dir);
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
        let (mut machine, _shutdown) = reopen(&dir);
        let (applied, _) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied.map(|id| id.index), Some(1), "and it was durable");
    }

    /// A membership entry records the exact `StoredMembership`, and it survives
    /// a reopen -- which is what `applied_state` must return at startup.
    #[tokio::test]
    async fn a_membership_entry_records_the_stored_membership_durably() {
        let dir = scratch_dir();
        let voters = [[1u8; 32], [2u8; 32], [3u8; 32]];
        let (mut machine, shutdown) = machine(&dir);

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
        let (mut machine, _shutdown) = reopen(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);

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
            let (mut machine, _shutdown) = machine(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);

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
        let (mut machine, _shutdown) = reopen(&dir);
        let (applied, _) = machine.applied_state().await.expect("applied state");
        assert_eq!(applied, None);
    }

    // ── Normal commands ─────────────────────────────────────────────────────

    /// A committed command persists its effects, its audit row, its session and
    /// both counters, and every one of them survives a reopen.
    #[tokio::test]
    async fn a_committed_command_persists_everything_it_wrote() {
        let dir = scratch_dir();
        let (mut machine, shutdown) = machine(&dir);

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
        let (machine, _shutdown) = reopen(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);

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
        let (mut machine, shutdown) = machine(&dir);

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
        let (machine, _shutdown) = reopen(&dir);
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

        let (mut machine, shutdown) = reopen(&dir);
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
        let (mut machine, _shutdown) = reopen(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);

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
        let (mut machine, _shutdown) = machine(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);

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
        let (mut machine, shutdown) = machine(&dir);

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
        let (machine, _shutdown) = reopen(&dir);
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
        let (mut machine, shutdown) = reopen(&dir);
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
        let (machine, _shutdown) = reopen(&dir);
        let state = published(&machine).await;
        let intent = &state.model.intents[&operation];
        assert_eq!(intent.status_code, PHASE_CANCELLED, "and it is durable");
        assert!(
            intent.completed_log_id.is_some(),
            "a terminal intent carries the id it completed at"
        );
        assert_eq!(state.model.last_applied.map(|id| id.index), Some(3));
    }

    // ── The batch ───────────────────────────────────────────────────────────

    /// A mixed batch produces one reply per entry, in order, and commits once.
    #[tokio::test]
    async fn a_mixed_batch_answers_every_entry_and_commits_together() {
        let dir = scratch_dir();
        let (mut machine, shutdown) = machine(&dir);

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
        let (mut machine, _shutdown) = reopen(&dir);
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
        let dir = scratch_dir();
        let (mut machine, shutdown) = machine(&dir);

        machine
            .apply([
                normal(1, acquire(1)),
                membership_entry(2, &[[1u8; 32], [2u8; 32], [3u8; 32]]),
                normal(3, acquire(2)),
                // A progress report naming an operation that does not exist:
                // a no-op that must still not touch membership authority.
                normal(
                    4,
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
        let (machine, _shutdown) = reopen(&dir);
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
            Some(2),
            "the native entry is what set it, not the report claiming index 99"
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
        let (mut machine, shutdown) = machine(&dir);
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
        let (machine, _shutdown) = reopen(&dir);
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
        let (mut machine, _shutdown) = machine(&dir);
        let before = published(&machine).await;
        let replies = machine.apply(Vec::<Entry>::new()).await.expect("apply");
        assert!(replies.is_empty());
        assert_eq!(published(&machine).await, before);
    }

    /// Once fatal, everything queued behind the failure is refused rather than
    /// served -- those commands were enqueued while the store still looked
    /// healthy.
    #[tokio::test]
    async fn a_fatal_store_refuses_every_later_command() {
        let dir = scratch_dir();
        let (mut machine, _shutdown) = machine(&dir);
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
        let err = CoordinationStateMachine::new(database).expect_err("validation refuses it");
        assert!(err.puts_durability_in_question(), "{err:?}");
    }
}
