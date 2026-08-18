//! Durable voter storage: redb schema, OpenRaft log and state machine,
//! snapshots (§11).
//!
//! Voter-only. A client evaluates no transition and stores no log, so
//! `coordination-client` compiles none of this.
//!
//! ## Shape
//!
//! | Module | Owns |
//! |---|---|
//! | [`error`] | the named §11.1 failures, kept distinct on purpose |
//! | [`identity`] | the crate-private storage-open identity and policy |
//! | [`schema`] | layout, lifetime lock, table/meta allowlists, the open coordinator |
//! | [`log`] | `RaftLogStorage` over `raft.redb` (Checkpoint B) |
//! | [`machine`] | `RaftStateMachine` over `state.redb` (Checkpoint C) |
//! | [`snapshot`] | the role-safe snapshot handle and catalog (Checkpoint D) |
//!
//! Everything here is crate-private. Slice 3 owns the public start
//! configuration that supplies verified genesis, authority, start mode, and
//! admission; exposing a storage type before then would fix a public contract
//! this slice cannot verify.
//!
//! ## Unreached-by-shipping-code allowances
//!
//! Each module below carries its own module-scoped `allow(dead_code)`, and
//! this one does not. A crate-root allowance would silently cover every
//! storage module added after it, and a test suite proves exercise, not
//! shipping reachability.
//!
//! **These do not come off in Checkpoint B, and an earlier note here was
//! wrong to say they would.** Nothing in a shipping build reaches
//! [`VoterStorage::open`] yet, because opening storage needs a verified
//! [`identity::StorageIdentity`] — group id, genesis hash, authority key and
//! domain — and the signed bootstrap that produces one is Slice 3's. The
//! public
//! [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination)
//! accepts a [`CoordinationConfig`](crate::coordination::host::CoordinationConfig)
//! that deliberately carries none of it, precisely so no field reads as
//! configured while nothing verifies it.
//!
//! Making the call reachable now would mean inventing that bootstrap here,
//! which is the Slice 3 boundary the plan draws. The allowances come off when
//! Slice 3 wires the voter runtime, not before.

pub(crate) mod error;
/// The OpenRaft storage-suite fixture. Test-only: it exists to hold one group
/// identity fixed across every store a builder makes, which is what
/// `transfer_snapshot` requires.
#[cfg(test)]
pub(crate) mod fixture;
pub(crate) mod identity;
pub(crate) mod log;
pub(crate) mod machine;
pub(crate) mod schema;
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;

use error::StorageResult;
use identity::{OpenPolicy, StorageIdentity};
use log::{ActorShutdown, CoordinationLogStore};
use schema::{DirectoryLock, OpenOutcome, StorageLayout};

/// A validated, locked storage directory with its log actor running.
///
/// Composition lives here rather than in [`schema`] because the open
/// coordinator must own each database by value while checking it, and
/// [`schema`] cannot hand one to an actor it does not know about without
/// depending on [`log`].
///
/// `state` is still a plain database: Checkpoint C is what gives it an actor.
///
/// The allowance is on this item, not on the module: a module-wide one here
/// would mask every later addition to the storage root, which is the thing the
/// per-module scoping above exists to avoid.
#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct VoterStorage {
    layout: StorageLayout,
    identity: StorageIdentity,
    log: CoordinationLogStore,
    state: redb::Database,
    outcome: OpenOutcome,
    /// Field order below is the shutdown order, and it is load-bearing.
    ///
    /// Rust drops fields in declaration order, so the actor is stopped and
    /// joined -- releasing its `Database` -- before the directory lock goes.
    /// Reversed, a restart would find the lock free but the database still
    /// open, and a second voter could be admitted while the first still holds
    /// the log.
    _shutdown: ActorShutdown,
    _lock: DirectoryLock,
}

#[allow(dead_code)]
impl VoterStorage {
    /// Open the storage directory and start the log actor.
    pub(crate) fn open(
        layout: StorageLayout,
        identity: StorageIdentity,
        policy: OpenPolicy,
    ) -> StorageResult<Self> {
        let parts = schema::open(layout, identity, policy)?.into_parts();
        let (log, shutdown) = CoordinationLogStore::new(parts.raft);
        Ok(Self {
            layout: parts.layout,
            identity: parts.identity,
            log,
            state: parts.state,
            outcome: parts.outcome,
            _shutdown: shutdown,
            _lock: parts.lock,
        })
    }

    pub(crate) fn log(&self) -> &CoordinationLogStore {
        &self.log
    }

    pub(crate) fn log_mut(&mut self) -> &mut CoordinationLogStore {
        &mut self.log
    }

    pub(crate) fn state(&self) -> &redb::Database {
        &self.state
    }

    pub(crate) fn layout(&self) -> &StorageLayout {
        &self.layout
    }

    pub(crate) fn identity(&self) -> &StorageIdentity {
        &self.identity
    }

    pub(crate) fn outcome(&self) -> OpenOutcome {
        self.outcome
    }
}
