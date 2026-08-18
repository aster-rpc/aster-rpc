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

// Checkpoint A builds the open coordinator; Checkpoints B and C are what call
// it, so in a non-test build nothing yet reaches `schema::open` or the errors
// it returns. The alternative — wiring a half-implemented log actor now purely
// to satisfy the lint — is exactly what the checkpoint order exists to
// prevent.
//
// Scoped to this module and temporary: Checkpoint B removes it, and the
// storage tests already exercise every item below, so a genuinely dead item
// would show up as an untested one.
#![allow(dead_code)]

pub(crate) mod error;
pub(crate) mod identity;
pub(crate) mod log;
pub(crate) mod machine;
pub(crate) mod schema;
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;
