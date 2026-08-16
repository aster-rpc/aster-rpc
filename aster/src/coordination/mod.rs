//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

pub mod contract;
pub mod ids;
pub mod rowstream;
pub mod service;

/// Host-side configuration for a voter. Needs `coordination`: it names a
/// storage directory and is accepted only by
/// [`builtin_coordination`](crate::rpc::AsterServerBuilder::builtin_coordination).
#[cfg(feature = "coordination")]
pub mod host;

/// The OpenRaft type configuration. Voter-only, so it needs `coordination`
/// rather than `coordination-client`.
#[cfg(feature = "coordination")]
pub mod typeconfig;
