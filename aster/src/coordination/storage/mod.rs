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
//! [`error`], [`identity`], [`schema`], and [`snapshot`] each carry their own
//! module-scoped `allow(dead_code)`. Checkpoint A builds the open coordinator
//! and the snapshot handle, and Checkpoints B, C and D are what call them, so
//! in a non-test build nothing yet reaches `schema::open`. Wiring a
//! half-implemented log actor now purely to satisfy the lint is what the
//! checkpoint order exists to prevent.
//!
//! The allowance is deliberately on each module and not on this one. A
//! crate-root allowance would silently cover every storage module added after
//! it — including the log and state-machine code Checkpoints B and C are
//! supposed to make reachable — and a test suite proves exercise, not shipping
//! reachability. Checkpoint B removes the allowance from `schema`; C and D
//! remove the rest.

pub(crate) mod error;
pub(crate) mod identity;
pub(crate) mod log;
pub(crate) mod machine;
pub(crate) mod schema;
pub(crate) mod snapshot;

pub(crate) use snapshot::CoordinationSnapshotFile;
