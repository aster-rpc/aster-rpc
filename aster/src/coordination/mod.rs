//! Coordination: the replicated register/lease group.
//!
//! `coordination-client` compiles the wire contract, the validated identifier
//! types, and (later) the client stub. `coordination` adds the voter — the
//! replicated state machine and its redb storage.

pub mod contract;
pub mod ids;
pub mod rowstream;
