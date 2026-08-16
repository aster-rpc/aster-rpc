//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

pub mod contract;
pub mod convert;
pub mod hash;
pub mod ids;
pub mod reason;
pub mod rowstream;
pub mod service;

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
/// §5.3.1 OpenRaft mirrors and §11 storage roots. Voter-only: they mirror
/// types a client never sees.
#[cfg(feature = "coordination")]
pub mod raft;

#[cfg(feature = "coordination")]
pub mod typeconfig;
