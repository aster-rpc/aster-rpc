//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

/// Source-level guards over the whole subsystem: the rules that cannot be
/// checked at runtime, with one inventory of every file they read.
#[cfg(all(test, feature = "coordination"))]
pub(crate) mod guards;

pub mod contract;
pub mod hash;
pub mod ids;
pub mod reason;
pub mod rowstream;
pub mod service;

/// The §8.4 verified admission source. Voter-only: a client is admitted *by* a
/// voter and consults no directory itself.
#[cfg(feature = "coordination")]
pub mod admission;

/// Host-side configuration for a voter. Needs `coordination`: it names a
/// storage directory and is accepted only by
/// [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination).
#[cfg(feature = "coordination")]
pub mod host;

/// The replicated state machine. Voter-only: a client evaluates no transition,
/// so `coordination-client` compiles none of it.
#[cfg(feature = "coordination")]
pub mod model;

/// The OpenRaft type configuration. Voter-only, so it needs `coordination`
/// rather than `coordination-client`.
/// §5.3.1 OpenRaft mirrors, the §5.3 wire messages, and the §11 storage roots.
///
/// Compiled by `coordination-client` as well as `coordination`: these are plain
/// Fory declarations with no `openraft` dependency, and `rowstream`'s snapshot
/// and recovery bodies nest the storage roots. Only the conversions to and from
/// real OpenRaft values need the voter feature, and they live in
/// [`super::convert`].
pub mod raft;

/// Conversions between OpenRaft values and the §5.3.1 mirrors. Voter-only: it
/// is the one place `openraft` types appear outside the type config.
#[cfg(feature = "coordination")]
pub mod convert;

/// The three start corridors and the §3.3 startup order (§7.2). Voter-only
/// and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod boot;

/// The authenticated Raft transport over Aster RPC (§5.3, §5.4). Voter-only
/// and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod network;

/// The voter runtime: pinned OpenRaft configuration, lifecycle and readiness
/// (§7.5, §7.6). Voter-only and crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod runtime;

/// Resolving a configured start mode into a verified storage binding (§7.2).
/// Crate-private: it produces a `StorageIdentity`, which is itself crate-private.
#[cfg(feature = "coordination")]
pub(crate) mod start;

#[cfg(feature = "coordination")]
pub mod typeconfig;

/// Durable redb storage: schema, log, state machine, snapshots (§11).
/// Voter-only and crate-private — Slice 3 owns the public start
/// configuration, so nothing here is nameable by a consumer yet.
#[cfg(feature = "coordination")]
pub(crate) mod storage;

/// The Slice 3 internal test harness (§S3-E, §S3-F). Never enabled by a
/// shipping profile; see the module docs for why it is one method.
#[cfg(feature = "coordination-testing")]
pub mod testing;

/// A handle on a running coordination voter (§3.3).
///
/// The public surface Slice 3 owns: readiness, and the lifecycle a caller
/// needs to interpret it. Obtained from
/// [`AsterServer::coordination`](crate::rpc::AsterServer::coordination); the
/// runtime itself stays owned by the server, so dropping this handle never
/// orphans a voter task.
#[cfg(feature = "coordination")]
#[derive(Clone)]
pub struct Coordination {
    readiness: runtime::readiness::Readiness,
    /// The seam's handle onto the running `Raft`.
    ///
    /// Gated with the seam that reads it, so a shipping build carries neither
    /// the field nor a method that could reach it. Cloning a `Raft` is an
    /// `Arc` bump, so this costs the handle nothing when it is present.
    #[cfg(feature = "coordination-testing")]
    raft: openraft::Raft<typeconfig::CoordinationTypeConfig>,
}

#[cfg(feature = "coordination")]
impl Coordination {
    pub(crate) fn of(rt: &runtime::owner::CoordinationRuntime) -> Self {
        Self {
            readiness: rt.readiness(),
            #[cfg(feature = "coordination-testing")]
            raft: rt.raft().clone(),
        }
    }

    /// Wait until this voter is `Ready`.
    ///
    /// **Level-triggered, and not a latch** (§7.6). It returns immediately if
    /// the node is already `Ready`, and otherwise waits for the next transition
    /// into `Ready`. It promises nothing about the future: a node may be
    /// `Ready` when this returns and `NoQuorum` a moment later. A caller
    /// needing a durable guarantee holds a fence.
    ///
    /// Returns an error if the node latches `Fatal` — no later message repairs
    /// that, so waiting would hang with the cause recorded nowhere — or if the
    /// runtime shuts down.
    pub async fn wait_ready(&self) -> crate::Result<()> {
        let mut readiness = self.readiness.clone();
        readiness.wait_ready().await.map_err(Into::into)
    }

    /// The §5.2 lifecycle code this node currently reports, without waiting.
    ///
    /// `1 Bootstrapping`, `2 Joining`, `3 Ready`, `4 NoQuorum`, `5 Draining`,
    /// `6 Fatal`.
    pub fn lifecycle_code(&self) -> i32 {
        self.readiness.code().code()
    }

    /// Whether this node holds **direct evidence** that a quorum is reachable
    /// (§7.6).
    ///
    /// `Some` on a leader, from OpenRaft's own `millis_since_quorum_ack`;
    /// `None` on a follower, which holds no such evidence. A plain `bool` would
    /// force a follower stranded in a minority partition to claim a quorum it
    /// cannot observe.
    pub fn quorum_available(&self) -> Option<bool> {
        self.readiness.view().quorum_available
    }
}
