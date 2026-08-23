//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

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
